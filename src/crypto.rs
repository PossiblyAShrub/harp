use std::{
    io::{self, Read, Write},
    net::TcpStream,
};

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use serde::{Serialize, de::DeserializeOwned};
use x25519_dalek::{PublicKey, StaticSecret};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

pub struct EncryptedStream {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    send_counter: u64,
    recv_counter: u64,
    is_client: bool,
}

impl EncryptedStream {
    pub fn handshake_client(stream: TcpStream) -> io::Result<Self> {
        Self::handshake(stream, true)
    }

    pub fn handshake_server(stream: TcpStream) -> io::Result<Self> {
        Self::handshake(stream, false)
    }

    fn handshake(mut stream: TcpStream, is_client: bool) -> io::Result<Self> {
        let _ = stream.set_nodelay(true);
        let mut secret_bytes = [0u8; 32];
        getrandom::fill(&mut secret_bytes)
            .map_err(|e| io::Error::other(format!("rng error: {e}")))?;
        let secret = StaticSecret::from(secret_bytes);
        let public = PublicKey::from(&secret);

        let mut peer_bytes = [0u8; 32];
        if is_client {
            stream.write_all(public.as_bytes())?;
            stream.flush()?;
            stream.read_exact(&mut peer_bytes)?;
        } else {
            stream.read_exact(&mut peer_bytes)?;
            stream.write_all(public.as_bytes())?;
            stream.flush()?;
        }

        let peer_public = PublicKey::from(peer_bytes);
        let shared = secret.diffie_hellman(&peer_public);
        let cipher = ChaCha20Poly1305::new_from_slice(shared.as_bytes())
            .map_err(|e| io::Error::other(format!("cipher init error: {e}")))?;

        Ok(Self {
            stream,
            cipher,
            send_counter: 0,
            recv_counter: 0,
            is_client,
        })
    }

    pub fn send<T: Serialize>(&mut self, msg: &T) -> io::Result<()> {
        let plaintext = serde_json::to_vec(msg).map_err(io::Error::other)?;
        let nonce_bytes = make_nonce(self.is_client, self.send_counter);
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("nonce counter overflow"))?;

        let ciphertext = self
            .cipher
            .encrypt(&Nonce::from(nonce_bytes), plaintext.as_slice())
            .map_err(|e| io::Error::other(format!("encrypt error: {e}")))?;

        let len: u32 = ciphertext
            .len()
            .try_into()
            .map_err(|_| io::Error::other("frame too large"))?;
        let mut frame = Vec::with_capacity(4 + ciphertext.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&ciphertext);
        self.stream.write_all(&frame)?;
        self.stream.flush()
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds maximum size",
            ));
        }

        let mut ciphertext = vec![0u8; len];
        self.stream.read_exact(&mut ciphertext)?;

        let nonce_bytes = make_nonce(!self.is_client, self.recv_counter);
        self.recv_counter = self
            .recv_counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("nonce counter overflow"))?;

        let plaintext = self
            .cipher
            .decrypt(&Nonce::from(nonce_bytes), ciphertext.as_slice())
            .map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("decrypt error: {e}"))
            })?;

        serde_json::from_slice(&plaintext)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode error: {e}")))
    }
}

fn make_nonce(from_client: bool, counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0] = u8::from(from_client);
    nonce[4..12].copy_from_slice(&counter.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn encrypted_stream_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut srv = EncryptedStream::handshake_server(stream).unwrap();
            let msg: String = srv.recv().unwrap();
            assert_eq!(msg, "hello encrypted world");
            srv.send(&42u64).unwrap();
        });

        let client_stream = TcpStream::connect(addr).unwrap();
        let mut cli = EncryptedStream::handshake_client(client_stream).unwrap();
        cli.send(&"hello encrypted world".to_string()).unwrap();
        let reply: u64 = cli.recv().unwrap();
        assert_eq!(reply, 42);

        server_handle.join().unwrap();
    }
}
