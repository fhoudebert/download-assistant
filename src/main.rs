//! # install-assistant v0.3
//!
//! TUI temps-réel (ratatui) + log fichier.
//!
//! ## Layout
//! ```
//! ┌─ install-assistant ──────────────────────────────────┐
//! │ [████████████░░░░░░░░] 2/3  60%   ✅ 1  ❌ 0         │  ← en-tête
//! ├──────────────────────────────────────────────────────┤
//! │    En cours  [══════════════════════════░░░] 68%      │  ← jauge active
//! ├──────────────────────────────────────────────────────┤
//! │  # │ Fichier          │ Destination  │ Taille  │ Statut │  ← tableau
//! │ ✓1 │ ggml-base.bin    │ build/whi... │ 147 MiB │ ✅ 201s │
//! │ ▶2 │ ggml-medium.bin  │ build/whi... │ 1.5 GiB │ ⬇ 68%  │
//! │  3 │ ggml-large-v3... │ build/whi... │       - │ ⏳      │
//! ├──────────────────────────────────────────────────────┤
//! │ [14:32:01] [1/3] ⬇  ggml-base.bin                   │  ← journal
//! │ [14:35:22] [1/3] ✅ OK (147 MiB en 201s)             │
//! ├──────────────────────────────────────────────────────┤
//! │ [q] Quitter  ·  fermeture auto dans 4s               │  ← pied de page
//! └──────────────────────────────────────────────────────┘
//! ```

use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Table},
    Frame, Terminal,
};
use reqwest::Client;
use tokio::io::AsyncWriteExt;

// ─── Constantes ──────────────────────────────────────────────────────────────

const DEFAULT_CSV: &str       = "downloads.csv";
const LOG_FILENAME: &str      = "install-assistant.log";
const HTTP_TIMEOUT_SECS: u64  = 7_200;
const STATE_UPDATE_MS: u64    = 200;   // fréquence de mise à jour progress (ms)
const AUTO_EXIT_SECS: u64     = 5;     // attente avant fermeture auto en fin de session
const MAX_LOG_LINES: usize    = 300;   // lignes max conservées en mémoire pour le TUI
const USER_AGENT: &str        = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

// ─── Types archives ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum ArchiveFormat { Zip, SevenZip, TarGz, TarBz2, TarXz, Gz }

#[derive(Debug)]
enum ExtractMode { Auto, Force(ArchiveFormat), No }

// ─── Entrée CSV ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct DownloadEntry {
    dest_dir:     String,
    url:          String,
    extract_mode: ExtractMode,
}

// ─── État runtime ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum EntryStatus {
    Pending,
    Downloading,
    Extracting,
    Done    { bytes: u64, elapsed_secs: f64 },
    Skipped,
    Error   (String),
}

#[derive(Debug, Clone)]
struct EntryState {
    index:       usize,
    url:         String,
    filename:    String,
    dest_dir:    String,
    format:      Option<ArchiveFormat>,
    status:      EntryStatus,
    bytes_done:  u64,
    bytes_total: Option<u64>,
    started_at:  Option<Instant>,
}

impl EntryState {
    /// Débit moyen depuis le début du téléchargement (B/s).
    fn speed_bps(&self) -> u64 {
        self.started_at.map(|t| {
            let e = t.elapsed().as_secs_f64();
            if e > 0.5 { (self.bytes_done as f64 / e) as u64 } else { 0 }
        }).unwrap_or(0)
    }

    /// Ratio de progression [0.0 – 1.0], None si taille inconnue.
    fn progress_ratio(&self) -> Option<f64> {
        self.bytes_total
            .filter(|&t| t > 0)
            .map(|t| (self.bytes_done as f64 / t as f64).clamp(0.0, 1.0))
    }
}

// ─── AppState (partagé TUI ↔ téléchargements) ────────────────────────────────

struct AppState {
    entries:   Vec<EntryState>,
    log_lines: Vec<String>,   // affiché dans le TUI
    all_done:  bool,
    log_file:  Option<fs::File>,
}

impl AppState {
    /// Écrit une ligne horodatée dans le log fichier ET dans le buffer TUI.
    fn log(&mut self, msg: &str) {
        // Fichier : date complète
        if let Some(f) = &mut self.log_file {
            let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
            let _ = writeln!(f, "{} {}", ts, msg);
        }
        // TUI : heure seule (économie de largeur)
        let ts_tui = Local::now().format("%H:%M:%S").to_string();
        self.log_lines.push(format!("[{}] {}", ts_tui, msg));
        if self.log_lines.len() > MAX_LOG_LINES {
            self.log_lines.remove(0);
        }
    }

    fn finished_count(&self) -> usize {
        self.entries.iter().filter(|e| {
            matches!(e.status, EntryStatus::Done{..} | EntryStatus::Skipped | EntryStatus::Error(_))
        }).count()
    }
    fn success_count(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e.status, EntryStatus::Done{..})).count()
    }
    fn error_count(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e.status, EntryStatus::Error(_))).count()
    }
    fn skipped_count(&self) -> usize {
        self.entries.iter().filter(|e| e.status == EntryStatus::Skipped).count()
    }
}

// ─── Point d'entrée ──────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let csv_path = args.get(1).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CSV));

    let base_dir: PathBuf = match args.get(2) {
        Some(p) => PathBuf::from(p),
        None    => env::current_dir().context("Impossible de lire le répertoire courant")?,
    };

    // ── Parsing CSV ──────────────────────────────────────────────────────────
    let entries = parse_csv(&csv_path)?;
    if entries.is_empty() {
        bail!("Aucune entrée valide dans «{}»", csv_path.display());
    }

    // ── Fichier de log (non-fatal si inaccessible) ───────────────────────────
    let log_path = base_dir.join(LOG_FILENAME);
    let log_file = OpenOptions::new().create(true).append(true).open(&log_path).ok();

    // ── Construction de l'état initial ───────────────────────────────────────
    let entry_states: Vec<EntryState> = entries.iter().enumerate().map(|(i, e)| {
        let filename = url_filename(&e.url);
        let format   = match &e.extract_mode {
            ExtractMode::No       => None,
            ExtractMode::Force(f) => Some(f.clone()),
            ExtractMode::Auto     => detect_format(&filename),
        };
        EntryState {
            index:       i + 1,
            url:         e.url.clone(),
            filename,
            dest_dir:    e.dest_dir.clone(),
            format,
            status:      EntryStatus::Pending,
            bytes_done:  0,
            bytes_total: None,
            started_at:  None,
        }
    }).collect();

    let n     = entry_states.len();
    let state = Arc::new(Mutex::new(AppState {
        entries:   entry_states,
        log_lines: Vec::new(),
        all_done:  false,
        log_file,
    }));

    // Entrées de log initiales
    {
        let mut s = state.lock().unwrap();
        s.log(&format!("=== install-assistant v{} ===", env!("CARGO_PKG_VERSION")));
        s.log(&format!("Base : {}  |  CSV : {}", base_dir.display(), csv_path.display()));
        s.log(&format!("Log  : {}", log_path.display()));
        s.log(&format!("{} fichier(s) à traiter", n));
        s.log("─────────────────────────────────────────────────");
    }

    // ── Runtime Tokio dans un thread dédié ───────────────────────────────────
    // (ratatui est bloquant et doit tourner dans le thread principal)
    let state_dl = state.clone();
    let base_dl  = base_dir.clone();
    let rt_thread = thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("Runtime Tokio")
            .block_on(run_downloads(state_dl, base_dl, n));
    });

    // ── TUI dans le thread principal ─────────────────────────────────────────
    run_tui(state)?;

    // Attente fin des téléchargements (TUI peut avoir été fermé avant)
    let _ = rt_thread.join();

    Ok(())
}

// ─── Boucle de téléchargements ───────────────────────────────────────────────

async fn run_downloads(state: Arc<Mutex<AppState>>, base_dir: PathBuf, total: usize) {
    let client = match Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(15))
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(c)  => c,
        Err(e) => {
            let mut s = state.lock().unwrap();
            s.log(&format!("❌ Client HTTP : {}", e));
            s.all_done = true;
            return;
        }
    };

    for idx in 0..total {
        let label = format!("[{}/{}]", idx + 1, total);

        // Lecture des données nécessaires (verrou court)
        let (url, dest_dir_s, filename, fmt) = {
            let s = state.lock().unwrap();
            let e = &s.entries[idx];
            (e.url.clone(), e.dest_dir.clone(), e.filename.clone(), e.format.clone())
        };

        let dest_dir  = base_dir.join(&dest_dir_s);
        let dest_file = dest_dir.join(&filename);

        // ── Création du répertoire ────────────────────────────────────────────
        if let Err(e) = fs::create_dir_all(&dest_dir) {
            let mut s = state.lock().unwrap();
            s.log(&format!("{} ❌ mkdir «{}» : {}", label, dest_dir.display(), e));
            s.entries[idx].status = EntryStatus::Error(format!("mkdir: {}", e));
            continue;
        }

        // ── Déjà présent ? ────────────────────────────────────────────────────
        if dest_file.exists() {
            let size = fs::metadata(&dest_file).map(|m| m.len()).unwrap_or(0);
            let mut s = state.lock().unwrap();
            s.log(&format!("{} ⏭  Déjà présent : {} ({})", label, filename, human_size(size)));
            s.entries[idx].status = EntryStatus::Skipped;
            continue;
        }

        // ── Téléchargement ────────────────────────────────────────────────────
        {
            let mut s = state.lock().unwrap();
            s.log(&format!("{} ⬇  {}", label, filename));
            s.log(&format!("     URL  : {}", url));
            s.log(&format!("     Vers : {}", dest_file.display()));
            s.entries[idx].status     = EntryStatus::Downloading;
            s.entries[idx].started_at = Some(Instant::now());
        }

        let bytes = match download_file_tracked(&client, &url, &dest_file, &state, idx).await {
            Ok(b)  => b,
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.log(&format!("{} ❌ Téléchargement : {:#}", label, e));
                s.entries[idx].status = EntryStatus::Error(format!("{:#}", e));
                let _ = fs::remove_file(dest_file.with_extension("tmp"));
                continue;
            }
        };

        // ── Extraction ────────────────────────────────────────────────────────
        if let Some(archive_fmt) = fmt {
            {
                let mut s = state.lock().unwrap();
                s.log(&format!("{} 📦 Extraction {:?}…", label, archive_fmt));
                s.entries[idx].status = EntryStatus::Extracting;
            }

            let arc  = dest_file.clone();
            let dir  = dest_dir.clone();
            let res  = tokio::task::spawn_blocking(move || extract_archive(&arc, &dir, &archive_fmt)).await;

            let elapsed = state.lock().unwrap().entries[idx]
                .started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);

            match res.map_err(|e| anyhow::anyhow!("thread: {}", e)).and_then(|r| r) {
                Ok(()) => {
                    let mut s = state.lock().unwrap();
                    s.log(&format!("{} ✅ {} extrait ({} en {:.0}s)", label, filename, human_size(bytes), elapsed));
                    s.entries[idx].status = EntryStatus::Done { bytes, elapsed_secs: elapsed };
                }
                Err(e) => {
                    let mut s = state.lock().unwrap();
                    s.log(&format!("{} ❌ Extraction : {:#}", label, e));
                    s.entries[idx].status = EntryStatus::Error(format!("extract: {:#}", e));
                }
            }
        } else {
            let elapsed = state.lock().unwrap().entries[idx]
                .started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            let mut s = state.lock().unwrap();
            s.log(&format!("{} ✅ {} ({} en {:.0}s)", label, filename, human_size(bytes), elapsed));
            s.entries[idx].status = EntryStatus::Done { bytes, elapsed_secs: elapsed };
        }
    }

    // Résumé final
    {
        let mut s = state.lock().unwrap();
        // Calcul des compteurs AVANT l'emprunt mutable de log()
        // (Rust interdit &self et &mut self simultanément dans un même appel)
        let ok  = s.success_count();
        let sk  = s.skipped_count();
        let err = s.error_count();
        s.log("─────────────────────────────────────────────────");
        s.log(&format!("=== Terminé : {} succès  {}  ignorés  {} erreur(s) ===", ok, sk, err));
        s.all_done = true;
    }
}

// ─── Téléchargement avec mise à jour de l'état ───────────────────────────────

async fn download_file_tracked(
    client: &Client,
    url:    &str,
    dest:   &Path,
    state:  &Arc<Mutex<AppState>>,
    idx:    usize,
) -> Result<u64> {
    let resp = client.get(url).send().await
        .with_context(|| format!("Connexion échouée : {}", url))?;

    if !resp.status().is_success() {
        bail!("HTTP {} — {}", resp.status(), url);
    }

    // Stocker la taille totale si connue
    let total = resp.content_length();
    { state.lock().unwrap().entries[idx].bytes_total = total; }

    // Écriture en flux vers un fichier temporaire
    let tmp  = dest.with_extension("tmp");
    let mut file = tokio::fs::File::create(&tmp).await
        .with_context(|| format!("Création «{}» impossible", tmp.display()))?;

    let mut written     = 0u64;
    let mut last_update = Instant::now();
    let mut stream      = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Flux réseau interrompu")?;
        file.write_all(&chunk).await.context("Écriture disque")?;
        written += chunk.len() as u64;

        // Mise à jour de l'état toutes les STATE_UPDATE_MS ms (évite la contention)
        if last_update.elapsed() >= Duration::from_millis(STATE_UPDATE_MS) {
            state.lock().unwrap().entries[idx].bytes_done = written;
            last_update = Instant::now();
        }
    }

    // Mise à jour finale (bytes exacts)
    state.lock().unwrap().entries[idx].bytes_done = written;

    file.flush().await.context("Flush")?;
    drop(file);

    // Renommage atomique → aucun fichier partiel en cas d'interruption
    tokio::fs::rename(&tmp, dest).await
        .with_context(|| format!("Rename «{}» → «{}»", tmp.display(), dest.display()))?;

    Ok(written)
}

// ─── TUI ─────────────────────────────────────────────────────────────────────

fn run_tui(state: Arc<Mutex<AppState>>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = tui_loop(&mut term, &state);

    // Restauration du terminal dans tous les cas (même en cas d'erreur)
    let _ = disable_raw_mode();
    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();

    result
}

fn tui_loop(
    term:  &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &Arc<Mutex<AppState>>,
) -> Result<()> {
    let mut done_at: Option<Instant> = None;

    loop {
        // Snapshot de done_at (Instant est Copy) pour la closure du draw
        let snap = done_at;
        term.draw(|f| {
            let s = state.lock().unwrap();
            render(f, &s, snap);
        })?;

        // Événements clavier (100 ms de timeout = refresh ~10 Hz)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
                        _ => {}
                    }
                }
            }
        }

        // Auto-exit après AUTO_EXIT_SECS secondes une fois tout terminé
        if state.lock().unwrap().all_done {
            let t = *done_at.get_or_insert_with(Instant::now);
            if t.elapsed().as_secs() >= AUTO_EXIT_SECS {
                return Ok(());
            }
        }
    }
}

// ─── Rendu TUI ───────────────────────────────────────────────────────────────

fn render(f: &mut Frame, state: &AppState, done_at: Option<Instant>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // en-tête : titre + progression globale
            Constraint::Length(3), // jauge téléchargement actif
            Constraint::Min(5),    // tableau des entrées (flexible)
            Constraint::Length(7), // panel journal (5 lignes + 2 bordures)
            Constraint::Length(1), // pied de page
        ])
        .split(f.area());

    render_header(f, chunks[0], state);
    render_active_gauge(f, chunks[1], state);
    render_table(f, chunks[2], state);
    render_log(f, chunks[3], state);
    render_footer(f, chunks[4], state, done_at);
}

// ── En-tête ───────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let total    = state.entries.len();
    let finished = state.finished_count();
    let ratio    = if total > 0 { finished as f64 / total as f64 } else { 0.0 };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" ⚙  install-assistant  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!(
                "  Progression globale : {} {:.0}%  ({}/{})",
                text_bar(ratio, 28), ratio * 100.0, finished, total
            )),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  ✅ {} succès  ⏭ {} ignorés  ❌ {} erreur(s)",
                    state.success_count(), state.skipped_count(), state.error_count()),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(para, area);
}

// ── Jauge téléchargement actif ────────────────────────────────────────────────

fn render_active_gauge(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    // Recherche de l'entrée en cours (Download ou Extraction)
    let active = state.entries.iter().find(|e| {
        matches!(e.status, EntryStatus::Downloading | EntryStatus::Extracting)
    });

    let (label, ratio, color) = match active {
        None if state.all_done => ("✅  Tous les téléchargements sont terminés".to_string(), 1.0_f64, Color::Green),
        None                   => ("⏳  Démarrage…".to_string(), 0.0_f64, Color::DarkGray),
        Some(e) => match &e.status {
            EntryStatus::Downloading => {
                let r   = e.progress_ratio().unwrap_or(0.0);
                let pct = format!("{:.0}%", r * 100.0);
                let lbl = format!(
                    "⬇  {}   {}  {}   {}",
                    trunc(&e.filename, 26),
                    human_size(e.bytes_done),
                    human_speed(e.speed_bps()),
                    pct,
                );
                (lbl, r, Color::Cyan)
            }
            EntryStatus::Extracting => (
                format!("📦  Extraction de {}…", e.filename),
                1.0, Color::Yellow,
            ),
            _ => ("".to_string(), 0.0, Color::DarkGray),
        },
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" En cours "))
        .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(Span::raw(label));

    f.render_widget(gauge, area);
}

// ── Tableau ───────────────────────────────────────────────────────────────────

fn render_table(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let header = Row::new(vec!["  #", "Fichier", "Destination", "Taille", "Statut"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .height(1);

    let rows: Vec<Row> = state.entries.iter().map(|e| {
        let (style, marker) = match &e.status {
            EntryStatus::Done{..}    => (Style::default().fg(Color::Green),               "✓"),
            EntryStatus::Error(_)    => (Style::default().fg(Color::Red),                 "✗"),
            EntryStatus::Skipped     => (Style::default().fg(Color::DarkGray),            "–"),
            EntryStatus::Downloading => (Style::default().fg(Color::Cyan),                "▶"),
            EntryStatus::Extracting  => (Style::default().fg(Color::Yellow),              "▶"),
            EntryStatus::Pending     => (Style::default().fg(Color::Gray),                " "),
        };

        // Colonne taille
        let size_cell = match &e.status {
            EntryStatus::Downloading => match e.bytes_total {
                Some(t) => format!("{} / {}", human_size(e.bytes_done), human_size(t)),
                None    => human_size(e.bytes_done),
            },
            EntryStatus::Done { bytes, .. } => human_size(*bytes),
            _ => e.bytes_total.map(human_size).unwrap_or_else(|| "-".into()),
        };

        // Colonne statut
        let status_cell = match &e.status {
            EntryStatus::Pending => "⏳ En attente".into(),
            EntryStatus::Downloading => {
                let r = e.progress_ratio()
                    .map(|p| format!(" {:.0}%", p * 100.0))
                    .unwrap_or_default();
                format!("⬇  {}{}", human_speed(e.speed_bps()), r)
            }
            EntryStatus::Extracting  => "📦 Extraction…".into(),
            EntryStatus::Done { elapsed_secs, .. } => format!("✅ {:.0}s", elapsed_secs),
            EntryStatus::Skipped     => "⏭  Présent".into(),
            EntryStatus::Error(msg)  => format!("❌ {}", trunc(msg, 16)),
        };

        Row::new(vec![
            Cell::from(format!("{}{}", marker, e.index)),
            Cell::from(trunc(&e.filename, 22)),
            Cell::from(trunc(&e.dest_dir, 18)),
            Cell::from(size_cell),
            Cell::from(status_cell),
        ]).style(style)
    }).collect();

    let widths = [
        Constraint::Length(3),   // #
        Constraint::Length(24),  // fichier
        Constraint::Length(20),  // destination
        Constraint::Length(20),  // taille
        Constraint::Min(14),     // statut
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Téléchargements "))
        .column_spacing(1);

    f.render_widget(table, area);
}

// ── Panel journal ─────────────────────────────────────────────────────────────

fn render_log(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    // Nombre de lignes visibles (hauteur - 2 bordures)
    let visible = (area.height as usize).saturating_sub(2);

    let items: Vec<ListItem> = state.log_lines
        .iter()
        .rev()
        .take(visible)
        .rev()
        .map(|l| ListItem::new(l.as_str()))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Journal "))
        .style(Style::default().fg(Color::DarkGray));

    f.render_widget(list, area);
}

// ── Pied de page ──────────────────────────────────────────────────────────────

fn render_footer(
    f:       &mut Frame,
    area:    ratatui::layout::Rect,
    state:   &AppState,
    done_at: Option<Instant>,
) {
    let (text, style) = if state.all_done {
        let remaining = AUTO_EXIT_SECS.saturating_sub(
            done_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
        );
        (
            format!(" ✅  Terminé  │  [q] / [Entrée] pour quitter  │  Fermeture auto dans {}s", remaining),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " [q] Quitter  │  Les téléchargements continuent en arrière-plan si vous quittez".to_string(),
            Style::default().fg(Color::White),
        )
    };

    f.render_widget(Paragraph::new(text).style(style), area);
}

// ─── Parsing CSV ─────────────────────────────────────────────────────────────

fn parse_csv(path: &Path) -> Result<Vec<DownloadEntry>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Lecture «{}» impossible", path.display()))?;

    Ok(content.lines().enumerate().filter_map(|(no, raw)| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { return None; }

        let cols: Vec<&str> = line.splitn(3, ',').collect();
        if cols.len() < 2 {
            eprintln!("Ligne {} ignorée (format invalide) : {}", no + 1, line);
            return None;
        }

        let dest = cols[0].trim().to_string();
        let url  = cols[1].trim().to_string();

        if dest.is_empty() || url.is_empty() {
            eprintln!("Ligne {} ignorée (champ vide)", no + 1);
            return None;
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            eprintln!("Ligne {} ignorée (URL invalide) : {}", no + 1, url);
            return None;
        }

        let extract_mode = parse_extract_col(cols.get(2).copied(), no + 1);
        Some(DownloadEntry { dest_dir: dest, url, extract_mode })
    }).collect())
}

fn parse_extract_col(col: Option<&str>, lineno: usize) -> ExtractMode {
    match col.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") | Some("auto") => ExtractMode::Auto,
        Some("no") | Some("none")      => ExtractMode::No,
        Some("zip")                    => ExtractMode::Force(ArchiveFormat::Zip),
        Some("7z") | Some("7zip")      => ExtractMode::Force(ArchiveFormat::SevenZip),
        Some("tar.gz") | Some("tgz")   => ExtractMode::Force(ArchiveFormat::TarGz),
        Some("tar.bz2") | Some("tbz2") => ExtractMode::Force(ArchiveFormat::TarBz2),
        Some("tar.xz") | Some("txz")   => ExtractMode::Force(ArchiveFormat::TarXz),
        Some("gz")                     => ExtractMode::Force(ArchiveFormat::Gz),
        Some(u) => {
            eprintln!("Ligne {} : format «{}» inconnu → auto", lineno, u);
            ExtractMode::Auto
        }
    }
}

fn detect_format(name: &str) -> Option<ArchiveFormat> {
    let n = name.to_lowercase();
    // Extensions composées avant les simples (ordre important)
    if      n.ends_with(".tar.gz")  || n.ends_with(".tgz")  { Some(ArchiveFormat::TarGz)    }
    else if n.ends_with(".tar.bz2") || n.ends_with(".tbz2") { Some(ArchiveFormat::TarBz2)   }
    else if n.ends_with(".tar.xz")  || n.ends_with(".txz")  { Some(ArchiveFormat::TarXz)    }
    else if n.ends_with(".zip")                              { Some(ArchiveFormat::Zip)       }
    else if n.ends_with(".7z")                               { Some(ArchiveFormat::SevenZip) }
    else if n.ends_with(".gz")                               { Some(ArchiveFormat::Gz)       }
    else                                                     { None                          }
}

// ─── Extraction ──────────────────────────────────────────────────────────────

fn extract_archive(archive: &Path, dest: &Path, fmt: &ArchiveFormat) -> Result<()> {
    match fmt {
        ArchiveFormat::Zip      => extract_zip(archive, dest),
        ArchiveFormat::SevenZip => extract_7z(archive, dest),
        ArchiveFormat::TarGz    => extract_tar(archive, dest, TarComp::Gz),
        ArchiveFormat::TarBz2   => extract_tar(archive, dest, TarComp::Bz2),
        ArchiveFormat::TarXz    => extract_tar(archive, dest, TarComp::Xz),
        ArchiveFormat::Gz       => extract_gz_single(archive, dest),
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let f = fs::File::open(archive).context("open zip")?;
    zip::ZipArchive::new(BufReader::new(f)).context("parse zip")?.extract(dest).context("extract zip")
}

fn extract_7z(archive: &Path, dest: &Path) -> Result<()> {
    sevenz_rust::decompress_file(archive, dest).map_err(|e| anyhow::anyhow!("7z: {}", e))
}

enum TarComp { Gz, Bz2, Xz }

fn extract_tar(archive: &Path, dest: &Path, comp: TarComp) -> Result<()> {
    let r = BufReader::new(fs::File::open(archive).context("open tar")?);
    match comp {
        TarComp::Gz  => tar::Archive::new(flate2::read::GzDecoder::new(r)).unpack(dest),
        TarComp::Bz2 => tar::Archive::new(bzip2::read::BzDecoder::new(r)).unpack(dest),
        TarComp::Xz  => tar::Archive::new(xz2::read::XzDecoder::new(r)).unpack(dest),
    }.context("unpack tar")
}

fn extract_gz_single(archive: &Path, dest: &Path) -> Result<()> {
    let stem = archive.file_stem().ok_or_else(|| anyhow::anyhow!("pas de stem"))?;
    let src  = fs::File::open(archive).context("open gz")?;
    let mut dec = flate2::read::GzDecoder::new(BufReader::new(src));
    let mut dst = fs::File::create(dest.join(stem)).context("create out")?;
    std::io::copy(&mut dec, &mut dst).context("decompress gz")?;
    Ok(())
}

// ─── Utilitaires ─────────────────────────────────────────────────────────────

fn url_filename(url: &str) -> String {
    url.split('/').last().unwrap_or("").split('?').next()
        .filter(|s| !s.is_empty()).unwrap_or("fichier").to_string()
}

fn human_size(bytes: u64) -> String {
    const K: u64 = 1024; const M: u64 = K * K; const G: u64 = K * M;
    match bytes {
        b if b >= G => format!("{:.2} GiB", b as f64 / G as f64),
        b if b >= M => format!("{:.1} MiB", b as f64 / M as f64),
        b if b >= K => format!("{:.0} KiB", b as f64 / K as f64),
        b           => format!("{} B", b),
    }
}

fn human_speed(bps: u64) -> String {
    if bps == 0 { return "-".into(); }
    const K: u64 = 1024; const M: u64 = K * K;
    match bps {
        b if b >= M => format!("{:.1} MiB/s", b as f64 / M as f64),
        b if b >= K => format!("{:.0} KiB/s", b as f64 / K as f64),
        b           => format!("{} B/s", b),
    }
}

/// Barre de progression textuelle : `[████░░░░░░]`
fn text_bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio * width as f64) as usize).min(width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

/// Troncature centrée avec `…` : `"long_file_name.bin"` → `"long_f…e.bin"`
fn trunc(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max { return s.to_string(); }
    if max < 4 { return chars[..max].iter().collect(); }
    let half = (max - 1) / 2;
    format!(
        "{}…{}",
        chars[..half].iter().collect::<String>(),
        chars[chars.len() - (max - half - 1)..].iter().collect::<String>()
    )
}
