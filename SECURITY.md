# Security Policy

## Supported versions

Security fixes are provided for the current **1.0.x** line. Older builds may not receive fixes.

## Reporting a vulnerability

Please **do not open a public issue** with exploit details, credentials, private data or a working attack against another installation.

Report privately to the repository owner through GitVerse: [@mibazarow](https://gitverse.ru/mibazarow).

Include, where possible:

- affected version and platform (macOS / Windows, architecture);
- steps to reproduce;
- impact and affected data or component;
- a minimal proof of concept with real secrets and client data removed;
- whether the issue is already public.

We aim to acknowledge reports within a reasonable time and will coordinate a fix and disclosure. This is a small independent project: there is no bug bounty and no guaranteed response time.

## Scope

In scope:

- local data storage, configuration and secret handling;
- IPC between the UI and the Rust backend;
- the agent loop, permissions/confirmation flow and tool execution;
- shell, SSH, browser, file and network tools;
- installers and update artifacts;
- path traversal, command injection and unsafe deserialization.

Out of scope:

- vulnerabilities in third-party LLM providers, GitVerse, Hugging Face or the operating system;
- attacks that require the user's own machine to be already compromised;
- social engineering;
- denial of service against your own local installation;
- issues that only affect unsupported or modified builds.

## Safe testing

Test only against your own installation and your own test data. Do not access, modify or exfiltrate another person's data, accounts or infrastructure. Do not use the project to attack third-party services.

## Secrets in reports

Remove API keys, tokens, passwords, private keys, client names, real domains and personal paths from screenshots and logs. If a secret may have leaked, revoke it first; do not send it to us.

## Commercial use

Commercial use of Project-S requires prior written permission from the copyright holder. See [LICENSE.md](LICENSE.md).
