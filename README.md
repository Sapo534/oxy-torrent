# Rust "Oxy-Torrent" Client 🦀🛰️

A minimalist, high-performance native torrent client written in Rust.

### Features:
- **Native GUI**: Fast and lightweight interface powered by **Slint** (Windows 11 Fluent style).
- **Pipelining**: Optimized request queue for maximum bandwidth saturation.
- **Multi-threaded Engine**: Parallel chunk downloads from multiple peers.
- **UDP & HTTP Support**: Compatible with modern UDP trackers (BEP 15) and classic HTTP trackers.
- **Session Persistence**: Automatically remembers and restores your downloads after restart.
- **File Resume**: Smart integrity check (SHA-1) to resume partial downloads without losing data.
- **Theme Support**: Includes both Dark and Light modes.

### How to run:
1. Download the latest `oxy-torrent.exe` from **Releases**.
2. Launch the application.
3. Go to **Settings** to select your download directory.
4. Use the **"Add Torrent"** button or drag-and-drop a `.torrent` file into the window.

## Development & AI Disclosure 🛠️💻

This repository represents a fresh restart of the Oxy-Torrent project, built with a focus on code quality, long-term maintainability, and architectural discipline.

- **Human-Centric Development:** Primary architecture, refactoring, and codebase implementation are led and written by human developers.
- **AI Assistance:** AI tools (such as LLMs) may be utilized as secondary aids for code review, documentation generation, or minor routine tasks, but human engineering drives the codebase.
- **Legacy Version:** The initial experimental AI-assisted prototype has been moved to the `master-legacy` branch for reference.
