use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramOption {
    pub label: String,
    pub program: String,
    pub display: String,
}

impl ProgramOption {
    pub fn new(label: impl Into<String>, program: impl Into<String>) -> Self {
        let program = program.into();
        Self {
            label: label.into(),
            display: program.clone(),
            program,
        }
    }
}

pub fn default_programs() -> Vec<ProgramOption> {
    vec![
        ProgramOption::new("ZSH", "zsh"),
        ProgramOption::new("Neovim", "nvim"),
        ProgramOption::new("lazygit", "lazygit"),
    ]
}

pub fn default_harp_config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("harp"))
}

pub fn parse_harp_file(contents: &str) -> Vec<ProgramOption> {
    let mut programs = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }

        let split = line.split_once('=').or_else(|| line.split_once(':'));

        let (label, program) = match split {
            Some((left, right)) => (left.trim(), strip_quotes(right.trim())),
            None => (line, line),
        };

        if !label.is_empty() && !program.is_empty() {
            programs.push(ProgramOption::new(label, program));
        }
    }

    programs
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn resolve_program(program: &str, base_dir: &Path) -> String {
    let path = Path::new(program);
    if path.is_absolute() || !program.contains('/') {
        program.to_string()
    } else {
        normalize_path(&base_dir.join(path))
            .to_string_lossy()
            .into_owned()
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            _ => out.push(component),
        }
    }
    out
}

pub fn load_programs(cwd: &Path, config_dir: Option<&Path>) -> Vec<ProgramOption> {
    let mut files = Vec::new();

    for dir in cwd.ancestors() {
        let candidate = dir.join(".harp");
        if candidate.is_file() {
            files.push(candidate);
        }
    }

    if let Some(dir) = config_dir {
        if let Ok(entries) = fs::read_dir(dir) {
            let mut global_files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "harp"))
                .collect();
            global_files.sort();
            files.extend(global_files);
        }
    }

    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for file in files {
        let base_dir = file.parent().unwrap_or_else(|| Path::new("/"));
        if let Ok(contents) = fs::read_to_string(&file) {
            for mut opt in parse_harp_file(&contents) {
                opt.program = resolve_program(&opt.program, base_dir);
                if seen.insert(opt.label.clone()) {
                    result.push(opt);
                }
            }
        }
    }

    if result.is_empty() {
        default_programs()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harp_test_{name}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_harp_file_lines() {
        let content = r#"
            # Comment
            // Another comment

            ZSH = zsh
            Neovim: "/home/aolse/.asdf/shims/nvim"
            lazygit = 'lazygit'
            htop
        "#;
        let parsed = parse_harp_file(content);
        assert_eq!(
            parsed,
            vec![
                ProgramOption::new("ZSH", "zsh"),
                ProgramOption::new("Neovim", "/home/aolse/.asdf/shims/nvim"),
                ProgramOption::new("lazygit", "lazygit"),
                ProgramOption::new("htop", "htop"),
            ]
        );
    }

    #[test]
    fn loads_nested_and_global_harp_files_with_precedence() {
        let root = temp_dir("nested");
        let child = root.join("project").join("sub");
        let global = root.join("global_config");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&global).unwrap();

        fs::write(root.join(".harp"), "Editor = vim\nShell = bash\n").unwrap();
        fs::write(
            child.join(".harp"),
            "Editor = /home/aolse/.asdf/shims/nvim\n",
        )
        .unwrap();
        fs::write(global.join("01-base.harp"), "Git = lazygit\nShell = zsh\n").unwrap();

        let loaded = load_programs(&child, Some(&global));
        assert_eq!(
            loaded,
            vec![
                ProgramOption::new("Editor", "/home/aolse/.asdf/shims/nvim"),
                ProgramOption::new("Shell", "bash"),
                ProgramOption::new("Git", "lazygit"),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_defaults_when_no_harp_files_exist() {
        let root = temp_dir("empty");
        let loaded = load_programs(&root, Some(&root.join("missing")));
        assert_eq!(loaded, default_programs());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_relative_paths_against_harp_file_directory() {
        let root = temp_dir("relative");
        let sub = root.join("nested").join("dir");
        fs::create_dir_all(&sub).unwrap();

        fs::write(
            root.join(".harp"),
            "Test = ./test.ysh\nSub = scripts/run.sh\nShell = zsh\nAbs = /usr/bin/env\n",
        )
        .unwrap();

        let loaded = load_programs(&sub, None);
        assert_eq!(
            loaded,
            vec![
                ProgramOption {
                    label: "Test".to_string(),
                    program: root.join("test.ysh").to_string_lossy().into_owned(),
                    display: "./test.ysh".to_string(),
                },
                ProgramOption {
                    label: "Sub".to_string(),
                    program: root.join("scripts/run.sh").to_string_lossy().into_owned(),
                    display: "scripts/run.sh".to_string(),
                },
                ProgramOption::new("Shell", "zsh"),
                ProgramOption::new("Abs", "/usr/bin/env"),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }
}
