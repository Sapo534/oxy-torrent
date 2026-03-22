#![windows_subsystem = "windows"]

use serde::{Deserialize, Serialize};
use sha1::{Sha1, Digest};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use tokio::net::{TcpStream, UdpSocket, TcpListener};
use tokio::fs::OpenOptions;
use urlencoding::encode_binary;
use rand::Rng;

slint::include_modules!();

// --- Configuration Storage ---

#[derive(Serialize, Deserialize, Clone)]
struct AppConfig {
    download_dir: String,
}

impl AppConfig {
    fn load() -> Self {
        std::fs::read_to_string("config.json")
            .and_then(|data| serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
            .unwrap_or_else(|_| AppConfig { download_dir: "downloads".to_string() })
    }
    fn save(&self) {
        if let Ok(data) = serde_json::to_string(self) {
            let _ = std::fs::write("config.json", data);
        }
    }
}

// --- Data Structures ---

#[derive(Debug, Deserialize, Clone)]
struct Torrent { 
    announce: String,
    #[serde(rename = "announce-list")]
    announce_list: Option<Vec<Vec<String>>>,
    info: Info 
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Info {
    name: String,
    #[serde(rename = "piece length")] piece_length: i64,
    pieces: serde_bytes::ByteBuf,
    length: Option<i64>,
    files: Option<Vec<File>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct File { length: i64, path: Vec<String> }

#[derive(Debug, Deserialize)]
struct TrackerResponse { peers: serde_bytes::ByteBuf }

struct DownloadManager {
    id: u32,
    info_hash: [u8; 20],
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
    peer_count: Mutex<usize>,
    hashes: Arc<Vec<[u8; 20]>>,
}

impl DownloadManager {
    fn update_speed(&self) {
        let now = Instant::now();
        let mut lt = self.last_tick_time.lock().unwrap();
        let mut lb = self.last_tick_bytes.lock().unwrap();
        let cur_b = *self.bytes_downloaded.lock().unwrap();
        let elapsed = now.duration_since(*lt).as_secs_f32();
        if elapsed >= 1.0 {
            let speed = ((cur_b.saturating_sub(*lb)) as f32 / elapsed) as u64;
            *self.current_speed.lock().unwrap() = speed;
            *lb = cur_b; *lt = now;
        }
    }

    fn get_ui_data(&self) -> TorrentUiData {
        let fin = *self.finished_count.lock().unwrap();
        let total = self.total_pieces;
        let is_done = fin >= total;
        let speed = if is_done { 0 } else { *self.current_speed.lock().unwrap() };
        let peers = *self.peer_count.lock().unwrap();
        
        TorrentUiData {
            id: self.id as i32,
            name: self.file_name.clone().into(),
            progress: (fin as f32 / total as f32) * 100.0,
            speed: if speed > 1024*1024 { format!("{:.2} MB/s", speed as f32 / 1024.0 / 1024.0).into() } 
                   else { format!("{:.2} KB/s", speed as f32 / 1024.0).into() },
            peers: format!("{}", peers).into(),
            status: if is_done { "Ready".into() } else { "Downloading".into() },
        }
    }
}

struct AppState {
    torrents: Mutex<Vec<Arc<DownloadManager>>>,
    download_dir: Mutex<String>,
}

// --- Utilities ---

fn get_random_port() -> u16 {
    rand::thread_rng().gen_range(10000..=65535)
}

async fn check_existing_pieces(path: &str, p_len: usize, hashes: &Arc<Vec<[u8; 20]>>, total_size: u64) -> (Vec<usize>, usize) {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return ((0..hashes.len()).collect(), 0),
    };
    let mut todo = Vec::new();
    let mut finished = 0;
    for i in 0..hashes.len() {
        let off = i * p_len;
        let clen = if i == hashes.len() - 1 { (total_size as usize) - off } else { p_len };
        let mut buf = vec![0u8; clen];
        let _ = file.seek(SeekFrom::Start(off as u64)).await;
        if file.read_exact(&mut buf).await.is_ok() && Sha1::digest(&buf).as_slice() == hashes[i] {
            finished += 1;
        } else {
            todo.push(i);
        }
    }
    (todo, finished)
}

// --- Networking ---

async fn announce_udp(tracker_url: &str, info_hash: [u8; 20], left: u64) -> Result<Vec<SocketAddrV4>, Box<dyn std::error::Error + Send + Sync>> {
    let host_port = tracker_url.replace("udp://", "").replace("/announce", "");
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(&host_port).await?;
    let tid: u32 = rand::random();
    let mut pkt = Vec::with_capacity(16);
    pkt.extend_from_slice(&0x41727101980u64.to_be_bytes()); pkt.extend_from_slice(&0u32.to_be_bytes()); pkt.extend_from_slice(&tid.to_be_bytes());
    socket.send(&pkt).await?;
    let mut buf = [0u8; 2048];
    let len = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf)).await??;
    if len < 16 { return Err("UDP fail".into()); }
    let cid = &buf[8..16];
    let mut ann = Vec::with_capacity(98);
    ann.extend_from_slice(cid); ann.extend_from_slice(&1u32.to_be_bytes()); ann.extend_from_slice(&tid.to_be_bytes());
    ann.extend_from_slice(&info_hash); ann.extend_from_slice(b"-RT0001-123456789012");
    ann.extend_from_slice(&0u64.to_be_bytes()); ann.extend_from_slice(&left.to_be_bytes()); ann.extend_from_slice(&0u64.to_be_bytes());
    ann.extend_from_slice(&0u32.to_be_bytes()); ann.extend_from_slice(&0u32.to_be_bytes()); ann.extend_from_slice(&rand::random::<u32>().to_be_bytes());
    ann.extend_from_slice(&(-1i32).to_be_bytes()); ann.extend_from_slice(&(6881u16).to_be_bytes());
    socket.send(&ann).await?;
    let len = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf)).await??;
    let mut peers = Vec::new();
    if len > 20 { for c in buf[20..len].chunks_exact(6) { peers.push(SocketAddrV4::new(Ipv4Addr::new(c[0],c[1],c[2],c[3]), u16::from_be_bytes([c[4],c[5]]))); } }
    Ok(peers)
}

async fn download_piece(s: &mut TcpStream, p_idx: u32, p_len: usize, hash: &[u8], m: Arc<DownloadManager>) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut data = vec![0u8; p_len];
    let (mut req, mut rec, mut inflight) = (0, 0, 0);
    while rec < p_len {
        while inflight < 12 && req < p_len {
            let len = std::cmp::min(16384, (p_len - req) as u32);
            let mut b = Vec::with_capacity(17);
            b.extend_from_slice(&13u32.to_be_bytes()); b.push(6);
            b.extend_from_slice(&p_idx.to_be_bytes()); b.extend_from_slice(&(req as u32).to_be_bytes()); b.extend_from_slice(&len.to_be_bytes());
            s.write_all(&b).await?; req += len as usize; inflight += 1;
        }
        let mut lb = [0u8; 4]; s.read_exact(&mut lb).await?;
        let ml = u32::from_be_bytes(lb); if ml == 0 { continue; }
        let id = s.read_u8().await?;
        if id == 7 {
            let _ = s.read_u32().await?; let start = s.read_u32().await? as usize;
            let blen = (ml - 9) as usize; s.read_exact(&mut data[start..start + blen]).await?;
            rec += blen; inflight -= 1; *m.bytes_downloaded.lock().unwrap() += blen as u64;
        } else {
            s.read_exact(&mut vec![0u8; (ml - 1) as usize]).await?;
            if id == 0 { return Err("Choke".into()); }
        }
    }
    if Sha1::digest(&data).as_slice() == hash { Ok(data) } else { Err("Hash fail".into()) }
}

async fn peer_worker(mut s: TcpStream, hash: [u8; 20], p_len: usize, f_len: u64, hashes: Arc<Vec<[u8; 20]>>, m: Arc<DownloadManager>, is_inbound: bool) {
    let _ = s.set_nodelay(true);
    if !is_inbound {
        let mut hs = Vec::with_capacity(68); hs.push(19); hs.extend_from_slice(b"BitTorrent protocol"); hs.extend_from_slice(&[0u8; 8]);
        hs.extend_from_slice(&hash); hs.extend_from_slice(b"-RT0001-123456789012");
        if s.write_all(&hs).await.is_err() || tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut [0u8; 68])).await.is_err() { return }
    }
    let _ = s.write_all(&[0, 0, 0, 1, 2]).await;
    let mut ok = false; let start = Instant::now();
    while start.elapsed().as_secs() < 15 {
        let mut lb = [0u8; 4];
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut lb)).await {
            let ml = u32::from_be_bytes(lb); if ml == 0 { continue }
            let id = s.read_u8().await.unwrap_or(0); // Fixed s vs stream
            if id == 1 { ok = true; break }
            let _ = s.read_exact(&mut vec![0u8; (ml - 1) as usize]).await;
        }
    }
    if !ok { return }
    { *m.peer_count.lock().unwrap() += 1; }
    let mut file = match OpenOptions::new().write(true).open(&m.save_path).await { Ok(f) => f, _ => { *m.peer_count.lock().unwrap() -= 1; return } };
    let file_mutex = Arc::new(tokio::sync::Mutex::new(file));
    while let Some(idx) = { let mut t = m.pieces_todo.lock().unwrap(); t.pop() } {
        let off = idx * p_len; let clen = if idx == m.total_pieces - 1 { (f_len as usize) - off } else { p_len };
        match download_piece(&mut s, idx as u32, clen, &hashes[idx], Arc::clone(&m)).await {
            Ok(d) => {
                let mut f = file_mutex.lock().await;
                let _ = f.seek(SeekFrom::Start(off as u64)).await;
                let _ = f.write_all(&d).await; *m.finished_count.lock().unwrap() += 1;
            }
            Err(_) => { m.pieces_todo.lock().unwrap().push(idx); break; }
        }
    }
    { *m.peer_count.lock().unwrap() -= 1; }
}

// --- App Logic ---

async fn start_torrent(data: Vec<u8>, state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let torrent: Torrent = serde_bencode::from_bytes(&data)?;
    let total_len = torrent.info.length.unwrap_or_else(|| torrent.info.files.as_ref().map(|fs| fs.iter().map(|f| f.length).sum()).unwrap_or(0)) as u64;
    let info_hash: [u8; 20] = Sha1::digest(&serde_bencode::to_bytes(&torrent.info)?).into();
    let save_path = format!("{}/{}", state.download_dir.lock().unwrap(), torrent.info.name);
    tokio::fs::create_dir_all("session").await.ok();
    tokio::fs::write(format!("session/{}.torrent", torrent.info.name), &data).await.ok();
    let num_pieces = torrent.info.pieces.len() / 20;
    let hashes = Arc::new((0..num_pieces).map(|i| { let mut h = [0u8; 20]; h.copy_from_slice(&torrent.info.pieces[i*20..(i+1)*20]); h }).collect::<Vec<_>>());
    let (todo, finished) = check_existing_pieces(&save_path, torrent.info.piece_length as usize, &hashes, total_len).await;
    let _ = OpenOptions::new().write(true).create(true).open(&save_path).await?.set_len(total_len).await;
    let manager = Arc::new(DownloadManager {
        id: rand::random(), info_hash, file_name: torrent.info.name.clone(), save_path,
        pieces_todo: Mutex::new(todo), total_pieces: num_pieces, piece_length: torrent.info.piece_length as usize,
        total_size: total_len, finished_count: Mutex::new(finished), bytes_downloaded: Mutex::new((finished * torrent.info.piece_length as usize) as u64),
        current_speed: Mutex::new(0), last_tick_bytes: Mutex::new(0), last_tick_time: Mutex::new(Instant::now()), peer_count: Mutex::new(0), hashes: Arc::clone(&hashes),
    });
    state.torrents.lock().unwrap().push(Arc::clone(&manager));
    let m_tick = Arc::clone(&manager);
    tokio::spawn(async move { loop { tokio::time::sleep(Duration::from_secs(1)).await; m_tick.update_speed(); if *m_tick.finished_count.lock().unwrap() >= m_tick.total_pieces { break } } });
    let mut trackers = vec![torrent.announce.clone()];
    if let Some(list) = torrent.announce_list { for sub in list { trackers.extend(sub); } }
    trackers.dedup();
    for url in trackers {
        let (m, h) = (Arc::clone(&manager), Arc::clone(&hashes));
        tokio::spawn(async move {
            let left = m.total_size - (*m.finished_count.lock().unwrap() as u64 * m.piece_length as u64);
            let peers = if url.starts_with("udp://") { announce_udp(&url, info_hash, left).await.unwrap_or_default() } 
                        else { 
                            let tr_req = format!("{}?info_hash={}&peer_id=-RT0001-123456789012&port=6881&uploaded=0&downloaded=0&left={}&compact=1", url, encode_binary(&info_hash), left);
                            reqwest::get(tr_req).await.ok().and_then(|_r| Some(vec![])).unwrap_or_default() 
                        };
            for addr in peers {
                let (mw, hw) = (Arc::clone(&m), Arc::clone(&h));
                tokio::spawn(async move { if let Ok(s) = TcpStream::connect(addr).await { peer_worker(s, info_hash, mw.piece_length, mw.total_size, hw, mw, false).await; } });
            }
        });
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::load();
    let state = Arc::new(AppState { torrents: Mutex::new(vec![]), download_dir: Mutex::new(config.download_dir) });
    let ui = AppWindow::new()?;
    let ui_handle = ui.as_weak();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt_guard = rt.enter();
    
    // Listener
    let s_listen = Arc::clone(&state);
    let l_port = get_random_port();
    rt.spawn(async move {
        let l = TcpListener::bind(format!("0.0.0.0:{}", l_port)).await.unwrap();
        loop { if let Ok((mut s, _)) = l.accept().await {
            let s_c = Arc::clone(&s_listen);
            tokio::spawn(async move {
                let mut hs = [0u8; 68]; if tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut hs)).await.is_err() { return }
                let manager = s_c.torrents.lock().unwrap().iter().find(|t| t.info_hash == hs[28..48]).cloned();
                if let Some(m) = manager {
                    let mut res = Vec::with_capacity(68); res.push(19); res.extend_from_slice(b"BitTorrent protocol"); res.extend_from_slice(&[0u8; 8]); res.extend_from_slice(&m.info_hash); res.extend_from_slice(b"-RT0001-123456789012");
                    if s.write_all(&res).await.is_ok() { peer_worker(s, m.info_hash, m.piece_length, m.total_size, Arc::clone(&m.hashes), m, true).await; }
                }
            });
        }}
    });

    // Restore
    let s_res = Arc::clone(&state);
    rt.spawn(async move {
        std::fs::create_dir_all("session").ok();
        if let Ok(files) = std::fs::read_dir("session") { for f in files.flatten() { if let Ok(d) = std::fs::read(f.path()) { let _ = start_torrent(d, Arc::clone(&s_res)).await; } } }
    });

    // UI loop
    let s_upd = Arc::clone(&state);
    let u_upd = ui_handle.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let ui_data: Vec<TorrentUiData> = s_upd.torrents.lock().unwrap().iter().map(|t| t.get_ui_data()).collect();
        let _ = u_upd.upgrade_in_event_loop(move |ui| { ui.set_torrent_list(slint::ModelRc::new(slint::VecModel::from(ui_data))); });
    });

    // Callbacks
    ui.on_add_torrent_clicked({
        let s = Arc::clone(&state);
        move || {
            let sc = Arc::clone(&s);
            if let Some(path) = rfd::FileDialog::new().add_filter("Torrent", &["torrent"]).pick_file() {
                if let Ok(data) = std::fs::read(path) { rt.spawn(async move { let _ = start_torrent(data, sc).await; }); }
            }
        }
    });

    let s_dir = Arc::clone(&state);
    let ui_dir = ui_handle.clone();
    ui.on_change_dir_clicked(move || {
        let s = Arc::clone(&s_dir);
        let u = ui_dir.clone();
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            let path_str = path.display().to_string();
            *s.download_dir.lock().unwrap() = path_str.clone();
            AppConfig { download_dir: path_str.clone() }.save();
            if let Some(ui) = u.upgrade() { ui.set_download_path(path_str.into()); }
        }
    });

    let s_del = Arc::clone(&state);
    ui.on_delete_torrent_clicked(move |id| {
        let mut t = s_del.torrents.lock().unwrap();
        if let Some(pos) = t.iter().position(|tm| tm.id == id as u32) {
            let m = t.remove(pos);
            let sp = m.save_path.clone();
            let sep = format!("session/{}.torrent", m.file_name);
            tokio::task::spawn_blocking(move || { let _ = std::fs::remove_file(sp); let _ = std::fs::remove_file(sep); });
        }
    });

    ui.set_download_path(state.download_dir.lock().unwrap().clone().into());
    ui.run()?;
    Ok(())
}