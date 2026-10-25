# Orion Assistant Capabilities & Architecture Notes

## 1. Current Capabilities
- **System Administration:** Package management (DNF/Flatpak), system updates, service control, journal auditing, and hardware/health monitoring.
- **Environment Control:** Managing network interfaces, Wi-Fi, Bluetooth, audio, power profiles, and display settings.
- **File & Workspace Management:** Reading, writing, searching, and organizing files within `/home/mrsilhouette/orion-workspace`.
- **Automation & Persistence:** Creating scheduled systemd timers and storing user preferences (`memory_save`) and conversation logs (`journal_write`).
- **Local AI Integration:** Interacting with local inference servers (Ollama), checking VRAM budgets, and loading/unloading models.

## 2. Limitations of the Current Setup
- **Sandboxed Execution:** Programs run in a strict bubblewrap sandbox (read-only outside writable roots, no network, filtered system calls).
- **No Persistent Shell:** Tool calls run single, discrete commands rather than maintaining an interactive, stateful terminal session.
- **Approval Policies:** High-privilege actions and file writes require explicit user approval (unless pre-approved via tuned settings).

## 3. Requirements for Full Software Building & Development
- **Persistent Terminal & Compiler Access:** An interactive shell loop with full access to build toolchains, compilers, and dependencies.
- **Network & Dependency Fetching:** Ability to download packages, libraries, and dependencies freely during builds.
- **Unrestricted Workspace:** Freedom to execute, test, and debug multi-file software projects iteratively.

## 4. Proposed Feature: Toggleable Developer Mode
- **Concept:** A security toggle in the OS/framework settings allowing users to switch between:
  1. *Safe Assistant Mode:* Strict sandboxing and restricted permissions for safe daily system administration.
  2. *Developer / Power-User Mode:* Full terminal access, compilers, network permissions, and fewer sandbox restrictions for autonomous software building and testing.
