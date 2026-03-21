#![windows_subsystem = "windows"] // Скрывает консоль в релизной сборке

use serde::{Deserialize, Serialize};
use sha1::{Sha1, Digest};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::fs::OpenOptions;
use urlencoding::encode_binary;

use axum::{
    extract::{Multipart, State, Path as AxumPath},
    response::Html,
    routing::{get, post},
    Json, Router,
};

// --- СТРУКТУРЫ ---

#[derive(Debug, Deserialize, Clone)]
struct Torrent {
    announce: String,
    info: Info,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Info {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: i64,
    pieces: serde_bytes::ByteBuf,
    length: Option<i64>,
    files: Option<Vec<File>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct File {
    length: i64,
    path: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TrackerResponse {
    peers: serde_bytes::ByteBuf,
}

struct DownloadManager {
    id: u32,
    file_name: String,
    save_path: String,
    pieces_todo: Mutex<Vec<usize>>,
    total_pieces: usize,
    piece_length: usize,
    total_size: u64,
    finished_count: Mutex<usize>,
    bytes_downloaded: Mutex<u64>,
    current_speed: Mutex<u64>,
    last_tick_bytes: Mutex<u64>,
    last_tick_time: Mutex<Instant>,
}

impl DownloadManager {
    fn update_speed(&self) {
        let now = Instant::now();
        let mut lt = self.last_tick_time.lock().unwrap();
        let mut lb = self.last_tick_bytes.lock().unwrap();
        let cur_b = *self.bytes_downloaded.lock().unwrap();
        let dur = now.duration_since(*lt).as_secs_f32();
        if dur >= 1.0 {
            *self.current_speed.lock().unwrap() = ((cur_b.saturating_sub(*lb)) as f32 / dur) as u64;
            *lb = cur_b; *lt = now;
        }
    }

    fn get_stats(&self) -> serde_json::Value {
        let fin = *self.finished_count.lock().unwrap();
        let total = self.total_pieces;
        let is_done = fin >= total;
        let speed = if is_done { 0 } else { *self.current_speed.lock().unwrap() };
        let percent = (fin as f32 / total as f32) * 100.0;
        
        serde_json::json!({
            "id": self.id,
            "name": self.file_name,
            "percent": percent,
            "speed": if speed > 1024*1024 { format!("{:.2} MB/s", speed as f32 / 1024.0 / 1024.0) } else { format!("{:.2} KB/s", speed as f32 / 1024.0) },
            "status": if is_done { "Готово" } else { "Скачивание" }
        })
    }
}

struct AppState {
    torrents: Mutex<Vec<Arc<DownloadManager>>>,
    download_dir: Mutex<String>,
}

// --- ЛОГИКА ПРОВЕРКИ СУЩЕСТВУЮЩИХ ФАЙЛОВ (RESUME) ---

async fn check_existing_pieces(path: &str, p_len: usize, hashes: &Arc<Vec<[u8; 20]>>, total_size: u64) -> (Vec<usize>, usize) {
    let mut todo = Vec::new();
    let mut finished = 0;
    
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return ((0..hashes.len()).collect(), 0),
    };

    for i in 0..hashes.len() {
        let off = i * p_len;
        let clen = if i == hashes.len() - 1 { (total_size as usize) - off } else { p_len };
        let mut buf = vec![0u8; clen];
        let _ = file.seek(SeekFrom::Start(off as u64)).await;
        if file.read_exact(&mut buf).await.is_ok() {
            if Sha1::digest(&buf).as_slice() == hashes[i] {
                finished += 1;
                continue;
            }
        }
        todo.push(i);
    }
    (todo, finished)
}

// --- СЕТЕВАЯ ЧАСТЬ ---

async fn download_piece(stream: &mut TcpStream, p_idx: u32, p_len: usize, hash: &[u8], m: Arc<DownloadManager>) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut data = vec![0u8; p_len];
    let (mut req, mut rec, mut inflight) = (0, 0, 0);
    let max_inflight = 12;

    while rec < p_len {
        while inflight < max_inflight && req < p_len {
            let len = std::cmp::min(16384, (p_len - req) as u32);
            let mut b = Vec::with_capacity(17);
            b.extend_from_slice(&13u32.to_be_bytes()); b.push(6);
            b.extend_from_slice(&p_idx.to_be_bytes()); b.extend_from_slice(&(req as u32).to_be_bytes()); b.extend_from_slice(&len.to_be_bytes());
            stream.write_all(&b).await?;
            req += len as usize; inflight += 1;
        }

        let mut lb = [0u8; 4];
        stream.read_exact(&mut lb).await?;
        let ml = u32::from_be_bytes(lb);
        if ml == 0 { continue; }
        let id = stream.read_u8().await?;
        if id == 7 {
            let _ = stream.read_u32().await?; let start = stream.read_u32().await? as usize;
            let blen = (ml - 9) as usize;
            stream.read_exact(&mut data[start..start + blen]).await?;
            rec += blen; inflight -= 1;
            *m.bytes_downloaded.lock().unwrap() += blen as u64;
        } else {
            stream.read_exact(&mut vec![0u8; (ml - 1) as usize]).await?;
            if id == 0 { return Err("Choke".into()); }
        }
    }
    if Sha1::digest(&data).as_slice() == hash { Ok(data) } else { Err("Hash mismatch".into()) }
}

async fn peer_worker(addr: SocketAddrV4, hash: [u8; 20], p_len: usize, f_len: u64, hashes: Arc<Vec<[u8; 20]>>, m: Arc<DownloadManager>) {
    let mut s = match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await { Ok(Ok(s)) => s, _ => return };
    let mut hs = Vec::with_capacity(68); hs.push(19); hs.extend_from_slice(b"BitTorrent protocol"); hs.extend_from_slice(&[0; 8]);
    hs.extend_from_slice(&hash); hs.extend_from_slice(b"-RT0001-123456789012");
    if s.write_all(&hs).await.is_err() || tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut [0u8; 68])).await.is_err() { return }
    let _ = s.write_all(&[0, 0, 0, 1, 2]).await;
    let mut unchoked = false; let start = Instant::now();
    while start.elapsed().as_secs() < 15 {
        let mut lb = [0u8; 4]; if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut lb)).await {
            let ml = u32::from_be_bytes(lb); if ml == 0 { continue }
            if s.read_u8().await.unwrap_or(0) == 1 { unchoked = true; break }
            let _ = s.read_exact(&mut vec![0u8; (ml - 1) as usize]).await;
        }
    }
    if !unchoked { return }
    let mut file = match OpenOptions::new().write(true).open(&m.save_path).await { Ok(f) => f, _ => return };
    while let Some(idx) = { let mut t = m.pieces_todo.lock().unwrap(); t.pop() } {
        let off = idx * p_len; let clen = if idx == m.total_pieces - 1 { (f_len as usize) - off } else { p_len };
        match download_piece(&mut s, idx as u32, clen, &hashes[idx], Arc::clone(&m)).await {
            Ok(d) => { let _ = file.seek(SeekFrom::Start(off as u64)).await; let _ = file.write_all(&d).await; *m.finished_count.lock().unwrap() += 1; }
            Err(_) => { m.pieces_todo.lock().unwrap().push(idx); return }
        }
    }
}

// --- ЛОГИКА ЗАПУСКА ТОРРЕНТА ---

async fn start_torrent(data: Vec<u8>, state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let torrent: Torrent = serde_bencode::from_bytes(&data).map_err(|e| e.to_string())?;
    let total_len = torrent.info.length.unwrap_or_else(|| torrent.info.files.as_ref().map(|fs| fs.iter().map(|f| f.length).sum()).unwrap_or(0)) as u64;
    let info_hash: [u8; 20] = Sha1::digest(&serde_bencode::to_bytes(&torrent.info).unwrap()).into();
    
    let save_path = format!("{}/{}", state.download_dir.lock().unwrap(), torrent.info.name);
    tokio::fs::write(format!("session/{}.torrent", torrent.info.name), &data).await.ok();

    let num_pieces = torrent.info.pieces.len() / 20;
    let hashes = Arc::new((0..num_pieces).map(|i| { let mut h = [0u8; 20]; h.copy_from_slice(&torrent.info.pieces[i*20..(i+1)*20]); h }).collect::<Vec<_>>());

    let (todo, finished) = check_existing_pieces(&save_path, torrent.info.piece_length as usize, &hashes, total_len).await;

    let mut f = OpenOptions::new().write(true).create(true).open(&save_path).await.map_err(|e| e.to_string())?;
    f.set_len(total_len).await.ok();

    let manager = Arc::new(DownloadManager {
        id: rand::random::<u32>(), file_name: torrent.info.name.clone(), save_path,
        pieces_todo: Mutex::new(todo), total_pieces: num_pieces,
        piece_length: torrent.info.piece_length as usize, total_size: total_len,
        finished_count: Mutex::new(finished), bytes_downloaded: Mutex::new((finished * torrent.info.piece_length as usize) as u64),
        current_speed: Mutex::new(0), last_tick_bytes: Mutex::new(0), last_tick_time: Mutex::new(Instant::now()),
    });

    state.torrents.lock().unwrap().push(Arc::clone(&manager));
    let m_tick = Arc::clone(&manager);
    tokio::spawn(async move { loop { tokio::time::sleep(Duration::from_secs(1)).await; m_tick.update_speed(); if *m_tick.finished_count.lock().unwrap() >= m_tick.total_pieces { break } } });

    let m_work = Arc::clone(&manager);
    tokio::spawn(async move {
        let url = format!("{}?info_hash={}&peer_id=-RT0001-123456789012&port=6881&uploaded=0&downloaded=0&left={}&compact=1", torrent.announce, encode_binary(&info_hash), total_len);
        if let Ok(res) = reqwest::get(url).await {
            if let Ok(body) = res.bytes().await {
                if let Ok(tr) = serde_bencode::from_bytes::<TrackerResponse>(&body) {
                    for c in tr.peers.chunks_exact(6) {
                        let addr = SocketAddrV4::new(Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]]));
                        let (m, h) = (Arc::clone(&m_work), Arc::clone(&hashes));
                        tokio::spawn(async move { peer_worker(addr, info_hash, torrent.info.piece_length as usize, total_len, h, m).await; });
                    }
                }
            }
        }
    });
    Ok(())
}

// --- ВЕБ-ОБРАБОТЧИКИ ---

async fn choose_dir_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let res = tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await.unwrap();
    if let Some(path) = res {
        let p = path.display().to_string();
        *state.download_dir.lock().unwrap() = p.clone();
        Json(serde_json::json!({"status": "ok", "path": p}))
    } else { Json(serde_json::json!({"status": "cancel"})) }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all("session").ok();
    let state = Arc::new(AppState { torrents: Mutex::new(vec![]), download_dir: Mutex::new("downloads".to_string()) });

    if let Ok(files) = std::fs::read_dir("session") {
        for f in files.flatten() {
            if let Ok(data) = std::fs::read(f.path()) { let _ = start_torrent(data, Arc::clone(&state)).await; }
        }
    }

    let s_web = Arc::clone(&state);
    tokio::spawn(async move {
        let app = Router::new()
            .route("/", get(|| async { Html(include_str!("index.html")) }))
            .route("/api/stats", get(|State(s): State<Arc<AppState>>| async move { Json(s.torrents.lock().unwrap().iter().map(|tm| tm.get_stats()).collect::<Vec<_>>()) }))
            .route("/api/upload", post(|State(s): State<Arc<AppState>>, mut mp: Multipart| async move {
                while let Ok(Some(f)) = mp.next_field().await { if let Ok(d) = f.bytes().await { let _ = start_torrent(d.to_vec(), Arc::clone(&s)).await; } }
                Json(serde_json::json!({"status":"ok"}))
            }))
            .route("/api/delete/:id", post(|State(s): State<Arc<AppState>>, AxumPath(id): AxumPath<u32>| async move {
                let mut torrents = s.torrents.lock().unwrap();
                
                // 1. Находим торрент в списке по ID, чтобы получить его данные
                if let Some(pos) = torrents.iter().position(|t| t.id == id) {
                    // Удаляем из вектора и получаем владение объектом
                    let manager = torrents.remove(pos);
                    
                    // 2. Путь к торрент-файлу в сессии
                    let session_path = format!("session/{}.torrent", manager.file_name);
                    
                    // 3. Удаляем файлы (используем spawn_blocking, так как удаление файлов — блокирующая операция)
                    let save_path = manager.save_path.clone();
                    tokio::task::spawn_blocking(move || {
                        // Удаляем метаданные (.torrent), чтобы он не восстановился после перезагрузки
                        let _ = std::fs::remove_file(session_path);
                        
                        // Удаляем сам скачанный файл (напр. .zip)
                        // ВНИМАНИЕ: Если ты хочешь оставить скачанный файл, удали строчку ниже
                        let _ = std::fs::remove_file(save_path);
                    });
                    
                    println!("Торрент {} и его файлы удалены.", manager.file_name);
                }

                Json(serde_json::json!({"status":"ok"}))
            }))
            .route("/api/settings", get(|State(s): State<Arc<AppState>>| async move { Json(serde_json::json!({"path": *s.download_dir.lock().unwrap()})) }))
            .route("/api/choose-dir", post(choose_dir_handler))
            .with_state(s_web);
        let l = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
        axum::serve(l, app).await.unwrap();
    });

    tokio::signal::ctrl_c().await?;
    Ok(())
}