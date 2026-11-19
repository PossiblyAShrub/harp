# Harp

Harp is an instrument for *quality* engineering, that happens to involve AI
agents. This means that the software engineer still is in command of how their
system behaves. The goal is to leverage advanced engineering capabilities
without removing the user from the loop.

## What is Harp

## Scopes

Harp focuses on ensuring the engineer is kept up to date the properties of
their system in the most useful way possible. In order to do this, we need to
set the scope of what is to be viewed by the user.

Scopes may be nested, and may have multiple projections. For example, a small
CLI tool will likely have the scope of just a view of the projects source code,
tests and documentation. But it could be more complex. A large company software
system may have a scope of the whole system interacting, containing scopes for
each web server codebase, application code. Some apps may want to be viewed
independently, or some scopes may want to contain multiple components so they
can be reasoned about.

Scopes are not meant to be all-encompassing, however. It is still important to
ensure the system stays a collection of files on a file system. We do not want
to re-engineer the whole operating system. Just the interface in which the
engineer sees the code.

## Projections

Scopes are a collection of projections. A projection is the output of an
application (eg. neovim in a directory, an issues list for a repo, a file tree,
etc.).

A scope is not just the sum of it's projection. Instead, scopes come annotated
with metadata about: where each projection runs, how to run each projection and
how to organize those projections.

Projections exist to provide the engineer with information about a scope. For
example, in a RL project, an agent may create a projection for a user to
visualize the environment and how a learned policy interacts with the
environment.

## The Why

LLM-powered systems are invariably capable of the autonomous reasoning skills
required to solve a significant number of software problems. This means that
for every software problem, with a sufficient number of LLM tokens that problem
can be solved. There remain two issues: 1) defining solved is itself a problem
that cannot be automated, 2) LLMs will always have a finite cost and thus there
is the meta-problem of determining where LLM tokens should be spent ^1.

In response to this thesis, Harp is designed as a tool to augment software
systems. Development practices which neglect the thesis invariably falter
under increased complexity, a poor definition of correct and a lack of insight
into how the system works. Should future software engineering endeavours seek
to responsibly integrate agentic systems into it's processes, it becomes
necessary to confront these 2 core issues.

Before delving too deeply into the process, I want to state a useful lemma. Let
us relate the relationship between a contract and it's implementation. A
sufficiently accurate contract is equivalent to it's implementation. Take, for
example, a function signature. In a crude language, one often writes `Int ->
Int -> Int`. This statement defines a constraint on the body of the function to
be generated. The system here is often *under-constrained* since it follows
than many distinct implementations are correct per this specification, but
rarely do those implementations match the intent. For a more precise
description of the function, such as "compute the area of a rectangle",
suddenly there exists a cleaner description of the intent. But formally we need
to define "area" and "rectangle". Increasing the detail requires information
that is equivalent to the definition of the function. This, our lemma: there
exists a tension between the specificity of our contracts and the ambiguity of
the intended definitions.

A common and significant problem in software engineering is maintaining a
precise definition of the intent behind a program. In a more complex,
non-trivial program, it can often be difficult to clearly specify the intended
behaviour of that program. For example, humans have developed complex legal
systems over thousands of years to clearly state the intent of their laws.
However, all systems eventually develop a legal system of judges who determine
the intent of the law. The element here is that intent is often abstract and
cannot be defined clearly ever. Software engineering works on the specific
invariant problem of ensuring that the intended behaviour of a program matches
that of the program's users.

Returning to (1) of our thesis, we claim that the process of discovering intent
cannot be automated. This is because the user is an abstract being that does
not exist. Real people, or systems, interfacing with our software are rough
matches to the "user". Put another way, the people using your software often do
not exactly know what they want from their software. It is the job of the software
engineer to divine the intent.

Agentic systems can move quickly, far more so than most prior code generation
systems, because they are often capable of deriving intent from vague
descriptions of a user's desires ^2. This allows for breakthroughs such as an
LLM's capability to "vibe-code". This process, however, breaks down without the
help of additional tools. As programs get larger, or the contexts in which they
function become more complex, it is no longer as efficient to keep all
knowledge within the current context. Instead, external systems need to be
used. Invariably this is having items such as an existing codebase or
documentation system. One major issue with documentation is that its loose
coupling to the actual program mean that correctness can break as the program
evolves. This means that the underlying code must be treated as the universal
source of truth.

Yet we return to the fundamental issue: care must be taken to ensure that the
code matches the intent. Furthermore, care must be taken to ensure that the
derived intent is entirely coherent.

Now let us highlight the (2) component of our thesis. Why is it that we select
between different reasoning levels when driving agentic systems? Often this
requires: a) an understanding of the problem's scope, direction and difficulty;
b) an understanding of the model's capabilities; and c) a budget in both time
and cost (tokens) that we are willing to allocate to this problem.

I argue, that should the agent-using instrument have a synergistic
understanding of the models and the problem (intent, as discussed), then the
reasoning trade-off should be more apparent to the instrument user and thus
actually representative.

This culminates in a lot of theory and rambling which defines what it is that
harp desires to solve:
1. Ensure that the intent of software is clearly maintained
2. Use this intent not just for proving correctness, but also aiding
   development

Now we face the question: how can the intent of a program be represented? We
could represent this intent as human language, however translating from human
language to concrete tests/proofs is an expensive and non-deterministic
process. The major flaw of natural language is the ambiguities present
throughout.

This is where I realise there exists a significant difference between my ideas
and those who have worked in software engineering for decades. I argue that
many tools, eg. class diagram languages, were an attempt at writing intent at a
high level. This is not helpful since, given the lemma from before, these
specifications are either too vague to be useful, or too detailed to be quick
to make. Instead, we take the approach of "just read the code bro".

So harp exists in a stark juxtaposition to the traditional agentic engineering
instruments. Most tools focus on a neat single interface: a chat process. It
demands that all the knowledge and reasoning work is done by an autonomous
agent. The human is removed entirely from the process. They are not encouraged
to join in, to apply their minds. They are treated as consumers of the
intelligence product. And this is great for those who wish to sell you more
tokens. But I find this to be a frustrating infantilization of the engineer.

Instead, harp will exist entirely centered around the code. The human is to
view and understand the system. It is an expectation. The should be able to
gleam a deep understanding through a lens unobscured by autonomous reasoning
systems. To hide that code is to hide the intent and it is to make the software
engineers job harder.

In making the code more visible, I hope that we liberate the engineer. It is
not meant to be an easier or simpler experience. It is meant to engage. And, as
a result, I expect the engineer to deeply understand their systems via their
interactions with the tool.

1: To extend (2) with a remark, it is becoming apparent that fronteir LLMs are
increasingly jagged in their knowledge, and the simple "Low", "Medium", "Max"
reasoning effort settings no longer scale to large problems.

2: In fact, I argue that they are often terrible at programming. From personal
experience with LLMs, they struggle to proceed beyond the median of code
quality. Many routines lack the sort of beauty and surprise that would generally
appear in that of a skilled programmer. The unfortunate reality, however, is
that this quality is sufficient for most if not all real problems. This may be why
LLMs have learnt to allocate little reasoning beyond the mean when producing code.
