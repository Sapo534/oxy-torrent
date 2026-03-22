# Rust "Oxy-Torrent" Client 🦀🛰️

## EN 
A minimalist, high-performance native torrent client written in Rust.

### Features:
- **Native GUI**: Fast and lightweight interface powered by **Slint** (Windows 11 Fluent style).
- **Pipelining**: Optimized request queue (32 blocks) for maximum bandwidth saturation.
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

## AI Disclosure 🤖💻

This project was developed through an intensive 4-hour pair-programming session between a human architect and the **Gemini 3.0 Flash Preview** neural network.

- **AI's Role:** Implementing the core BitTorrent protocol, low-level networking (TCP/UDP), multi-threaded architecture, and Slint UI logic.
- **Human's Role:** Architectural design, OS-level debugging (Windows), UI/UX polishing, and quality assurance.
- **Status:** Experimental prototype. It is fully functional for daily tasks but may require further refactoring for stability.

---

## RU
Минималистичный и высокопроизводительный нативный торрент-клиент на Rust.

### Особенности:
- **Нативный GUI**: Быстрый и легкий интерфейс на базе **Slint** (в стиле Windows 11 Fluent).
- **Pipelining**: Оптимизированная очередь запросов (32 блока) для максимальной скорости загрузки.
- **Многопоточность**: Скачивание кусков параллельно от разных пиров одновременно.
- **Поддержка UDP и HTTP**: Работает с современными UDP-трекерами (BEP 15) и классическими HTTP-серверами.
- **Сохранение сессий**: Автоматически запоминает и восстанавливает список загрузок при перезапуске.
- **Докачка (Resume)**: Умная проверка целостности (SHA-1) для продолжения загрузки без потери данных.
- **Поддержка тем**: Наличие темного и светлого режимов интерфейса.

### Как запустить:
1. Скачайте последнюю версию `oxy-torrent.exe` в разделе **Releases**.
2. Запустите приложение.
3. Перейдите в **Settings**, чтобы выбрать папку для сохранения файлов.
4. Нажмите **"Add Torrent"** или просто перетащите `.torrent` файл в окно программы.

## ИИ 🤖💻

Этот проект был полностью разработан в ходе интенсивной 4-часовой сессии совместной работы человека и нейросети **Gemini 3.0 Flash Preview**.

- **Роль ИИ:** Написание сетевого кода, реализация протокола BitTorrent (TCP/UDP), создание многопоточной архитектуры и логики интерфейса Slint.
- **Моя роль:** Проектирование архитектуры, отладка на уровне ОС (Windows), доработка UI/UX и постановка задач.
- **Статус:** Экспериментальный прототип. Полностью работоспособен и пригоден для скачивания файлов, но код может требовать рефакторинга для достижения стабильности.
