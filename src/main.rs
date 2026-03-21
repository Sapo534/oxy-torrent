use serde::{Deserialize, Serialize};
use sha1::{Sha1, Digest};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::fs::OpenOptions;
use urlencoding::encode_binary;
use axum::{extract::State, response::Html, routing::get, Json, Router};

// --- 1. СТРУКТУРЫ ДАННЫХ ---

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

// --- 2. МЕНЕДЖЕР ЗАГРУЗКИ ---

struct DownloadManager {
    id: usize,
    file_name: String,
    pieces_todo: Mutex<Vec<usize>>,
    total_pieces: usize,
    piece_length: usize,
    total_size: u64,
    
    // Статистика
    finished_count: Mutex<usize>,
    bytes_downloaded: Mutex<u64>,
    current_speed: Mutex<u64>, // байт в сек
    last_tick_bytes: Mutex<u64>,
    last_tick_time: Mutex<Instant>,
}

impl DownloadManager {
    fn get_next_piece(&self) -> Option<usize> {
        let mut todo = self.pieces_todo.lock().unwrap();
        todo.pop()
    }

    fn mark_finished(&self) { // Убери параметр bytes
        let mut count = self.finished_count.lock().unwrap();
        *count += 1;
    }

    fn return_piece(&self, idx: usize) {
        let mut todo = self.pieces_todo.lock().unwrap();
        todo.push(idx);
    }

    fn update_speed(&self) {
        let now = Instant::now();
        let mut last_time = self.last_tick_time.lock().unwrap();
        let mut last_bytes = self.last_tick_bytes.lock().unwrap();
        let current_bytes = *self.bytes_downloaded.lock().unwrap();
        
        let duration = now.duration_since(*last_time).as_secs_f32();
        if duration >= 1.0 {
            let delta_bytes = current_bytes.saturating_sub(*last_bytes);
            let speed = (delta_bytes as f32 / duration) as u64;
            *self.current_speed.lock().unwrap() = speed;
            *last_bytes = current_bytes;
            *last_time = now;
        }
    }

    fn get_stats(&self) -> serde_json::Value {
        let finished = *self.finished_count.lock().unwrap();
        let speed = *self.current_speed.lock().unwrap();
        let downloaded = *self.bytes_downloaded.lock().unwrap();
        let percent = (finished as f32 / self.total_pieces as f32) * 100.0;
        
        let remaining = self.total_size.saturating_sub(downloaded);
        let eta = if speed > 0 { remaining / speed } else { 0 };

        serde_json::json!({
            "id": self.id,
            "name": self.file_name,
            "percent": percent,
            "speed": format_speed(speed),
            "eta": format_eta(eta),
            "status": if finished == self.total_pieces { "Готово" } else { "Скачивание" }
        })
    }
}

fn format_speed(bps: u64) -> String {
    if bps > 1024 * 1024 { format!("{:.2} MB/s", bps as f32 / 1024.0 / 1024.0) }
    else { format!("{:.2} KB/s", bps as f32 / 1024.0) }
}

fn format_eta(secs: u64) -> String {
    if secs == 0 { return "∞".into(); }
    if secs > 3600 { format!("{}ч {}м", secs / 3600, (secs % 3600) / 60) }
    else { format!("{}м {}с", secs / 60, secs % 60) }
}

// --- 3. ГЛОБАЛЬНЫЙ РЕЕСТР ---

struct AppState {
    torrents: Mutex<Vec<Arc<DownloadManager>>>,
}

// --- 4. ВЕБ-ОБРАБОТЧИКИ ---

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn stats_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let torrents = state.torrents.lock().unwrap();
    let stats: Vec<serde_json::Value> = torrents.iter().map(|t| t.get_stats()).collect();
    Json(serde_json::json!(stats))
}

// --- 5. СЕТЕВАЯ ЛОГИКА (БЛОКИ И ПИРЫ) ---

async fn download_piece(
    stream: &mut TcpStream,
    piece_idx: u32,
    piece_len: usize,
    expected_hash: &[u8],
    manager: Arc<DownloadManager>,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut data = vec![0u8; piece_len];
    let mut requested = 0;
    let mut received = 0;
    let block_size = 16384;
    let max_in_flight = 8; // Размер окна (сколько запросов висит в сети одновременно)
    let mut in_flight = 0;

    // Цикл пока не получим все байты куска
    while received < piece_len {
        // 1. Наполняем "очередь" запросов до max_in_flight
        while in_flight < max_in_flight && requested < piece_len {
            let to_request = std::cmp::min(block_size, (piece_len - requested) as u32);
            
            // Формируем запрос
            let mut req = Vec::with_capacity(17);
            req.extend_from_slice(&13u32.to_be_bytes());
            req.push(6); // Request
            req.extend_from_slice(&piece_idx.to_be_bytes());
            req.extend_from_slice(&(requested as u32).to_be_bytes());
            req.extend_from_slice(&to_request.to_be_bytes());
            
            stream.write_all(&req).await?;
            
            requested += to_request as usize;
            in_flight += 1;
        }

        // 2. Читаем ответы от пира
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let msg_len = u32::from_be_bytes(len_buf);
        if msg_len == 0 { continue; }

        let msg_id = stream.read_u8().await?;
        
        match msg_id {
            7 => { // PIECE (Данные блока)
                let _idx = stream.read_u32().await?;
                let begin = stream.read_u32().await? as usize;
                let block_data_len = (msg_len - 9) as usize;
                
                // Читаем сами данные напрямую в нужное место буфера
                stream.read_exact(&mut data[begin..begin + block_data_len]).await?;
                
                received += block_data_len;
                in_flight -= 1; // Один запрос выполнен, место в очереди освободилось

                // Обновляем общий счетчик байтов для GUI
                {
                    let mut total = manager.bytes_downloaded.lock().unwrap();
                    *total += block_data_len as u64;
                }
            }
            0 => return Err("Choke".into()), // Пир нас заблокировал - выходим
            _ => {
                // Пропускаем все остальные сообщения (Have, Bitfield и т.д.)
                let payload_len = (msg_len - 1) as usize;
                let mut junk = vec![0u8; payload_len];
                stream.read_exact(&mut junk).await?;
            }
        }
    }

    // 3. Проверка хеша всего куска
    let mut hasher = Sha1::new();
    hasher.update(&data);
    if hasher.finalize().as_slice() == expected_hash {
        Ok(data)
    } else {
        Err("Hash mismatch".into())
    }
}

async fn peer_worker(
    peer_addr: SocketAddrV4,
    info_hash: [u8; 20],
    my_peer_id: [u8; 20],
    piece_length: usize,
    total_file_len: u64,
    pieces_hashes: Arc<Vec<[u8; 20]>>,
    manager: Arc<DownloadManager>,
) {
    let mut stream = match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(peer_addr)).await {
        Ok(Ok(s)) => s,
        _ => return,
    };

    let mut hs = Vec::with_capacity(68);
    hs.push(19); hs.extend_from_slice(b"BitTorrent protocol"); hs.extend_from_slice(&[0; 8]);
    hs.extend_from_slice(&info_hash); hs.extend_from_slice(&my_peer_id);
    if stream.write_all(&hs).await.is_err() { return; }
    let mut hs_res = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut hs_res)).await.is_err() { return; }

    let _ = stream.write_all(&[0, 0, 0, 1, 2]).await;

    // Ждем Unchoke
    let mut unchoked = false;
    let start = Instant::now();
    while start.elapsed().as_secs() < 15 {
        let mut len_buf = [0u8; 4];
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut len_buf)).await {
            let msg_len = u32::from_be_bytes(len_buf);
            if msg_len == 0 { continue; }
            let msg_id = stream.read_u8().await.unwrap_or(0);
            if msg_id == 1 { unchoked = true; break; }
            let _ = stream.read_exact(&mut vec![0u8; (msg_len - 1) as usize]).await;
        }
    }
    if !unchoked { return; }

    let mut file = match OpenOptions::new().write(true).open(&manager.file_name).await {
        Ok(f) => f,
        _ => return,
    };

    while let Some(idx) = manager.get_next_piece() {
        let offset = idx * piece_length;
        let cur_piece_len = if idx == manager.total_pieces - 1 { (total_file_len as usize) - offset } else { piece_length };

        match download_piece(&mut stream, idx as u32, cur_piece_len, &pieces_hashes[idx], Arc::clone(&manager)).await {
            Ok(data) => {
                let _ = file.seek(SeekFrom::Start(offset as u64)).await;
                let _ = file.write_all(&data).await;
                manager.mark_finished();
            }
            Err(_) => { manager.return_piece(idx); return; }
        }
    }
}

// --- 6. ЗАПУСК ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bencode_data = std::fs::read("test.torrent")?;
    let torrent: Torrent = serde_bencode::from_bytes(&bencode_data)?;
    let total_len = torrent.info.length.unwrap_or_else(|| {
        torrent.info.files.as_ref().map(|fs| fs.iter().map(|f| f.length).sum()).unwrap_or(0)
    }) as u64;

    let info_encoded = serde_bencode::to_bytes(&torrent.info)?;
    let mut hasher = Sha1::new();
    hasher.update(&info_encoded);
    let info_hash: [u8; 20] = hasher.finalize().into();

    let num_pieces = torrent.info.pieces.len() / 20;
    let mut hashes = Vec::new();
    for i in 0..num_pieces {
        let mut h = [0u8; 20];
        h.copy_from_slice(&torrent.info.pieces[i*20..(i+1)*20]);
        hashes.push(h);
    }
    let pieces_hashes = Arc::new(hashes);

    // Файл
    let mut f = OpenOptions::new().write(true).create(true).open(&torrent.info.name).await?;
    f.set_len(total_len).await?;
    drop(f);

    // Менеджер
    let manager = Arc::new(DownloadManager {
        id: 1,
        file_name: torrent.info.name.clone(),
        pieces_todo: Mutex::new((0..num_pieces).rev().collect()),
        total_pieces: num_pieces,
        piece_length: torrent.info.piece_length as usize,
        total_size: total_len,
        finished_count: Mutex::new(0),
        bytes_downloaded: Mutex::new(0),
        current_speed: Mutex::new(0),
        last_tick_bytes: Mutex::new(0),
        last_tick_time: Mutex::new(Instant::now()),
    });

    // Реестр
    let state = Arc::new(AppState {
        torrents: Mutex::new(vec![Arc::clone(&manager)]),
    });

    // Фоновая задача обновления скорости
    let m_clone = Arc::clone(&manager);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            m_clone.update_speed();
        }
    });

    // Веб-сервер
    let s_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/api/stats", get(stats_handler))
            .with_state(s_clone);
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
        println!("GUI: http://localhost:3000");
        axum::serve(listener, app).await.unwrap();
    });

    // Пиры
    let my_peer_id = *b"-RT0001-123456789012";
    let tracker_url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&compact=1",
        torrent.announce, encode_binary(&info_hash), urlencoding::encode("-RT0001-123456789012"), total_len
    );
    let res = reqwest::get(tracker_url).await?.bytes().await?;
    let peers_raw: TrackerResponse = serde_bencode::from_bytes(&res)?;
    let peers = peers_raw.peers.chunks_exact(6).map(|c| {
        SocketAddrV4::new(Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]]))
    }).collect::<Vec<_>>();

    println!("Качаем...");
    let mut tasks = vec![];
    for peer in peers.into_iter().take(40) {
        let m = Arc::clone(&manager);
        let h = Arc::clone(&pieces_hashes);
        tasks.push(tokio::spawn(async move {
            peer_worker(peer, info_hash, my_peer_id, torrent.info.piece_length as usize, total_len, h, m).await;
        }));
    }

    for t in tasks { let _ = t.await; }
    Ok(())
}