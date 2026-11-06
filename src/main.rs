#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod git;
mod store;

use eframe::egui::{self, Color32, RichText};
use git::RepoInfo;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::{Config, Profile, Store};

const GREEN: Color32 = Color32::from_rgb(80, 190, 110);
const YELLOW: Color32 = Color32::from_rgb(230, 190, 60);
const BLUE: Color32 = Color32::from_rgb(90, 150, 230);
const RED: Color32 = Color32::from_rgb(230, 80, 80);

struct App {
    store: Store,
    cfg: Config,
    infos: HashMap<PathBuf, RepoInfo>,
    pending: usize,
    tx: Sender<(PathBuf, RepoInfo)>,
    rx: Receiver<(PathBuf, RepoInfo)>,
    selected: Option<PathBuf>,
    filter: String,
    dirty_only: bool,
    add_input: String,
    scan_input: String,
    msg: String,
    last_refresh: Instant,
    started: bool,
    profile_edit: Option<ProfileEdit>,
    confirm_delete: bool,
    group_by_folder: bool,
    applied_font: f32,
}

enum Row {
    Header { key: String, title: String, full: String, count: usize, attention: usize, collapsed: bool },
    Repo(PathBuf),
}

enum ProfileEdit {
    New(String),
    Rename(String),
}

impl App {
    fn new() -> Self {
        let (store, cfg) = Store::open();
        let (tx, rx) = channel();
        Self {
            store,
            cfg,
            infos: HashMap::new(),
            pending: 0,
            tx,
            rx,
            selected: None,
            filter: String::new(),
            dirty_only: false,
            add_input: String::new(),
            scan_input: String::new(),
            msg: String::new(),
            last_refresh: Instant::now(),
            started: false,
            profile_edit: None,
            confirm_delete: false,
            group_by_folder: true,
            applied_font: 0.0,
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.pending > 0 || self.cfg.repos().is_empty() {
            return;
        }
        let repos = self.cfg.repos().clone();
        self.pending = repos.len();
        self.last_refresh = Instant::now();
        let n = repos.len().min(4);
        let queue = Arc::new(Mutex::new(VecDeque::from(repos)));
        for _ in 0..n {
            let q = queue.clone();
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || loop {
                let next = q.lock().unwrap().pop_front();
                let Some(p) = next else { break };
                let info = git::inspect(&p);
                let _ = tx.send((p, info));
                ctx.request_repaint();
            });
        }
    }

    fn add_repo(&mut self, p: PathBuf) -> bool {
        if self.cfg.repos().contains(&p) {
            return false;
        }
        self.cfg.repos_mut().push(p);
        true
    }

    fn save(&mut self) {
        self.store.save(&self.cfg);
    }
}

fn status_color(i: &RepoInfo) -> Color32 {
    if i.error.is_some() || i.conflicted > 0 {
        RED
    } else if i.is_dirty() {
        YELLOW
    } else if i.ahead > 0 || i.behind > 0 {
        BLUE
    } else {
        GREEN
    }
}

/// "2 ahead", "3 behind" or "2 ahead · 3 behind" (plain text: arrow glyphs
/// are missing from egui's default font).
fn sync_text(ahead: u32, behind: u32) -> String {
    match (ahead, behind) {
        (0, 0) => "in sync".into(),
        (a, 0) => format!("{a} ahead"),
        (0, b) => format!("{b} behind"),
        (a, b) => format!("{a} ahead · {b} behind"),
    }
}

/// Show a folder in the OS file manager. Only launches the file manager with
/// the folder as an argument (no shell, nothing inside the folder is executed).
fn open_folder(p: &std::path::Path) -> Result<(), String> {
    if !p.is_dir() {
        return Err("Folder not found.".into());
    }
    // a leading "./" stops a relative path starting with '-' being read as an option
    let target = if p.is_absolute() { p.to_path_buf() } else { std::path::Path::new(".").join(p) };
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let prog = "xdg-open";
    let mut child = std::process::Command::new(prog)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not open folder ({prog}: {e})"))?;
    std::thread::spawn(move || {
        let _ = child.wait(); // reap the process
    });
    Ok(())
}

/// Scale every text style relative to egui's defaults (14 px body).
fn apply_font(ctx: &egui::Context, size: f32) {
    let scale = size / 14.0;
    let mut style = (*ctx.style()).clone();
    for (ts, def) in egui::Style::default().text_styles.iter() {
        if let Some(f) = style.text_styles.get_mut(ts) {
            f.size = def.size * scale;
        }
    }
    ctx.set_style(style);
}

/// Last two path components, e.g. "projects1/clientA", for compact group headers.
fn short_dir(p: &std::path::Path) -> String {
    let parts: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let n = parts.len();
    if n == 0 {
        p.display().to_string()
    } else {
        parts[n.saturating_sub(2)..].join("/")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}...")
    }
}

fn clean_path(s: &str) -> PathBuf {
    PathBuf::from(s.trim().trim_matches('"').trim_matches('\''))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // collect results from workers
        while let Ok((p, info)) = self.rx.try_recv() {
            self.infos.insert(p, info);
            self.pending = self.pending.saturating_sub(1);
        }

        // initial + automatic refresh
        let missing = self.cfg.repos().iter().any(|p| !self.infos.contains_key(p));
        if !self.started || (missing && self.pending == 0) {
            // first run, or the profile/repo list has repos we haven't inspected yet
            self.started = true;
            self.refresh(ctx);
        } else if self.cfg.auto_refresh
            && self.last_refresh.elapsed() >= Duration::from_secs(self.cfg.interval_secs.max(10))
        {
            self.refresh(ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(1));

        let mut cfg_changed = false;
        let mut want_refresh = false;

        if (self.applied_font - self.cfg.font_size).abs() > f32::EPSILON {
            apply_font(ctx, self.cfg.font_size);
            self.applied_font = self.cfg.font_size;
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("RepoWatch");
                ui.label(RichText::new("read-only").small().color(GREEN));
                ui.separator();
                if ui
                    .add_enabled(self.pending == 0, egui::Button::new("Refresh"))
                    .clicked()
                {
                    want_refresh = true;
                }
                if self.pending > 0 {
                    ui.spinner();
                    ui.label(format!("{} left", self.pending));
                }
                ui.separator();
                if ui.checkbox(&mut self.cfg.auto_refresh, "Auto").changed() {
                    cfg_changed = true;
                }
                let mut secs = self.cfg.interval_secs;
                if ui
                    .add(egui::DragValue::new(&mut secs).range(10..=3600).suffix(" s"))
                    .changed()
                {
                    self.cfg.interval_secs = secs;
                    cfg_changed = true;
                }
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter…").desired_width(140.0));
                ui.checkbox(&mut self.dirty_only, "Needs attention only");
                ui.checkbox(&mut self.group_by_folder, "Group by folder");
                ui.separator();
                ui.label("Font");
                let mut fs = self.cfg.font_size;
                if ui.small_button("A-").clicked() {
                    fs -= 1.0;
                }
                ui.add(egui::DragValue::new(&mut fs).range(9.0..=32.0).speed(0.2).max_decimals(0).suffix(" px"));
                if ui.small_button("A+").clicked() {
                    fs += 1.0;
                }
                let fs = fs.clamp(9.0, 32.0);
                if (fs - self.cfg.font_size).abs() > f32::EPSILON {
                    self.cfg.font_size = fs;
                    cfg_changed = true;
                }
            });
            // profile tabs: each profile is its own group of repos
            ui.horizontal_wrapped(|ui| {
                ui.label("Profiles:");
                let mut switch_to = None;
                for (idx, p) in self.cfg.profiles.iter().enumerate() {
                    let label = format!("{} ({})", p.name, p.repos.len());
                    if ui.selectable_label(idx == self.cfg.active, label).clicked() {
                        switch_to = Some(idx);
                    }
                }
                if let Some(idx) = switch_to {
                    if idx != self.cfg.active {
                        self.cfg.active = idx;
                        self.selected = None;
                        self.confirm_delete = false;
                        self.profile_edit = None;
                        cfg_changed = true;
                    }
                }
                ui.separator();
                if let Some(mut edit) = self.profile_edit.take() {
                    let is_new = matches!(edit, ProfileEdit::New(_));
                    let txt = match &mut edit {
                        ProfileEdit::New(s) | ProfileEdit::Rename(s) => s,
                    };
                    let resp = ui.add(egui::TextEdit::singleline(txt).hint_text("profile name").desired_width(160.0));
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = ui.button("OK").clicked() || enter;
                    let cancel = ui.button("Cancel").clicked();
                    let name = txt.trim().to_string();
                    if ok && !name.is_empty() {
                        let active = self.cfg.active;
                        let dup = self
                            .cfg
                            .profiles
                            .iter()
                            .enumerate()
                            .any(|(i, p)| p.name.eq_ignore_ascii_case(&name) && (is_new || i != active));
                        if dup {
                            self.msg = "A profile with that name already exists.".into();
                            self.profile_edit = Some(edit);
                        } else if is_new {
                            self.cfg.profiles.push(Profile { name, repos: Vec::new() });
                            self.cfg.active = self.cfg.profiles.len() - 1;
                            self.selected = None;
                            cfg_changed = true;
                        } else {
                            self.cfg.profiles[active].name = name;
                            cfg_changed = true;
                        }
                    } else if !cancel {
                        self.profile_edit = Some(edit);
                    }
                } else if self.confirm_delete {
                    ui.colored_label(RED, "Delete this profile? (your repo folders are not touched)");
                    if ui.button("Yes, delete").clicked() && self.cfg.profiles.len() > 1 {
                        let a = self.cfg.active;
                        self.cfg.profiles.remove(a);
                        self.cfg.active = a.min(self.cfg.profiles.len() - 1);
                        self.selected = None;
                        self.confirm_delete = false;
                        cfg_changed = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete = false;
                    }
                } else {
                    if ui.button("+ New profile").clicked() {
                        self.profile_edit = Some(ProfileEdit::New(String::new()));
                    }
                    if ui.button("Rename").clicked() {
                        let n = self.cfg.profiles[self.cfg.active].name.clone();
                        self.profile_edit = Some(ProfileEdit::Rename(n));
                    }
                    if ui
                        .add_enabled(self.cfg.profiles.len() > 1, egui::Button::new("Delete profile"))
                        .on_hover_text("Removes the profile from RepoWatch only. Nothing on disk is touched.")
                        .clicked()
                    {
                        self.confirm_delete = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_input)
                        .hint_text("path to a git repo")
                        .desired_width(300.0),
                );
                if ui.button("Add repo").clicked() {
                    let p = clean_path(&self.add_input);
                    if !p.join(".git").exists() {
                        self.msg = "That folder is not a git repository (no .git found).".into();
                    } else if self.add_repo(p) {
                        self.add_input.clear();
                        self.msg = "Repository added.".into();
                        cfg_changed = true;
                        want_refresh = true;
                    } else {
                        self.msg = "Already in the list.".into();
                    }
                }
                ui.separator();
                ui.add(
                    egui::TextEdit::singleline(&mut self.scan_input)
                        .hint_text("folder to scan for repos")
                        .desired_width(300.0),
                );
                if ui.button("Scan folder").clicked() {
                    let root = clean_path(&self.scan_input);
                    if !root.is_dir() {
                        self.msg = "Not a folder.".into();
                    } else {
                        let found = git::scan(&root, 5);
                        let mut added = 0;
                        for p in &found {
                            if self.add_repo(p.clone()) {
                                added += 1;
                            }
                        }
                        self.msg = format!("Found {} repos, added {} new.", found.len(), added);
                        if added > 0 {
                            cfg_changed = true;
                            want_refresh = true;
                        }
                    }
                }
            });
            if let Some(e) = &self.store.error {
                ui.colored_label(RED, e);
            } else if !self.msg.is_empty() {
                ui.label(RichText::new(&self.msg).weak());
            }
            ui.add_space(4.0);
        });

        // sorted + filtered view
        let mut paths: Vec<PathBuf> = self.cfg.repos().clone();
        paths.sort_by_key(|p| {
            self.infos
                .get(p)
                .map(|i| i.name.to_lowercase())
                .unwrap_or_else(|| p.to_string_lossy().to_lowercase())
        });
        let needle = self.filter.to_lowercase();
        let visible: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| {
                let info = self.infos.get(p);
                let name = info
                    .map(|i| i.name.to_lowercase())
                    .unwrap_or_else(|| p.to_string_lossy().to_lowercase());
                let text_ok = needle.is_empty() || name.contains(&needle) || p.to_string_lossy().to_lowercase().contains(&needle);
                let attn_ok = !self.dirty_only
                    || info.map(|i| i.error.is_some() || i.is_dirty() || i.ahead > 0 || i.behind > 0).unwrap_or(false);
                text_ok && attn_ok
            })
            .collect();

        // bottom summary
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            // only the active profile's repos
            let current: Vec<&RepoInfo> = self.cfg.repos().iter().filter_map(|p| self.infos.get(p)).collect();
            let total = self.cfg.repos().len();
            let dirty = current.iter().filter(|i| i.is_dirty()).count();
            let unpushed = current.iter().filter(|i| i.ahead > 0).count();
            let behind = current.iter().filter(|i| i.behind > 0).count();
            let errs = current.iter().filter(|i| i.error.is_some()).count();
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{total} repos · {dirty} with local changes · {unpushed} unpushed · {behind} behind · {errs} errors"
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("data: {}", self.store.dir.display())).small().weak());
                });
            });
        });

        // detail panel
        let mut remove: Option<PathBuf> = None;
        let mut open_req: Option<PathBuf> = None;
        if let Some(sel) = self.selected.clone() {
            egui::SidePanel::right("detail").min_width(360.0).show(ctx, |ui| {
                if let Some(i) = self.infos.get(&sel) {
                    ui.heading(&i.name);
                    ui.label(RichText::new(i.path.display().to_string()).small().weak());
                    ui.horizontal(|ui| {
                        if ui.small_button("Open folder").clicked() {
                            open_req = Some(i.path.clone());
                        }
                        if ui.small_button("Copy path").clicked() {
                            ui.ctx().copy_text(i.path.display().to_string());
                        }
                        if ui
                            .small_button("Remove from list")
                            .on_hover_text("Only forgets it in RepoWatch. Your files are never touched.")
                            .clicked()
                        {
                            remove = Some(sel.clone());
                        }
                    });
                    ui.separator();
                    if let Some(e) = &i.error {
                        ui.colored_label(RED, e);
                    } else {
                        ui.label(format!("Branch: {}", i.branch));
                        match &i.upstream {
                            Some(u) => ui.label(format!("Upstream: {u}   ({}, as of last fetch)", sync_text(i.ahead, i.behind))),
                            None if i.detached => ui.label("Detached HEAD"),
                            None => ui.label("No upstream configured"),
                        };
                        ui.label(format!(
                            "Staged {} · Modified {} · Untracked {} · Conflicts {} · Stashes {}",
                            i.staged, i.modified, i.untracked, i.conflicted, i.stashes
                        ));
                        ui.separator();
                        ui.strong("Remotes");
                        if i.remotes.is_empty() {
                            ui.label(RichText::new("none configured (local-only repo)").weak());
                        }
                        for (name, url) in &i.remotes {
                            ui.label(RichText::new(format!("{name}: {url}")).monospace().small());
                            ui.horizontal(|ui| {
                                if ui.small_button("Copy URL").clicked() {
                                    ui.ctx().copy_text(url.clone());
                                }
                                if let Some(w) = git::web_url(url) {
                                    if ui.small_button("Open in browser").clicked() {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(w));
                                    }
                                }
                            });
                        }
                        ui.separator();
                        ui.strong("Working tree changes");
                        egui::ScrollArea::vertical().id_salt("chg").max_height(220.0).show(ui, |ui| {
                            if i.changes.is_empty() {
                                ui.label(RichText::new("clean").color(GREEN));
                            }
                            for c in &i.changes {
                                ui.label(RichText::new(c).monospace());
                            }
                        });
                        ui.separator();
                        ui.strong("Recent commits");
                        egui::ScrollArea::vertical().id_salt("log").show(ui, |ui| {
                            for r in &i.recent {
                                ui.label(RichText::new(r).monospace().small());
                            }
                        });
                    }
                } else {
                    ui.label("Not loaded yet…");
                }
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.cfg.repos().is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.heading("No repositories yet");
                    ui.label("Paste a repo path above, or scan a folder like D:\\projects to find them all.");
                });
                return;
            }
            let mut clicked: Option<PathBuf> = None;
            let mut toggle: Option<String> = None;

            // flat list or one collapsible group per parent folder
            let mut rows: Vec<Row> = Vec::new();
            if self.group_by_folder {
                let mut groups: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> = Default::default();
                for p in &visible {
                    let parent = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
                    groups.entry(parent).or_default().push(p.clone());
                }
                let profile = &self.cfg.profiles[self.cfg.active].name;
                for (parent, members) in groups {
                    let key = format!("{profile}\u{1}{}", parent.display());
                    let collapsed = self.cfg.collapsed.contains(&key);
                    let attention = members
                        .iter()
                        .filter_map(|m| self.infos.get(m))
                        .filter(|i| i.error.is_some() || i.is_dirty() || i.ahead > 0 || i.behind > 0)
                        .count();
                    rows.push(Row::Header {
                        key,
                        title: short_dir(&parent),
                        full: parent.display().to_string(),
                        count: members.len(),
                        attention,
                        collapsed,
                    });
                    if !collapsed {
                        rows.extend(members.into_iter().map(Row::Repo));
                    }
                }
            } else {
                rows.extend(visible.iter().cloned().map(Row::Repo));
            }

            egui::ScrollArea::both().show(ui, |ui| {
                egui::Grid::new("repos").striped(true).num_columns(10).spacing([14.0, 6.0]).show(ui, |ui| {
                    for h in ["", "Repo", "Branch", "Sync", "Changes", "Stash", "Remote", "Last commit", "When", ""] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for row in &rows {
                        let p = match row {
                            Row::Header { key, title, full, count, attention, collapsed } => {
                                // "+"/"-" text instead of arrow glyphs (missing from egui's default font)
                                let b = ui.small_button(if *collapsed { "+" } else { "-" });
                                let mut text = format!("{title}  ({count})");
                                if *attention > 0 {
                                    text.push_str(&format!("  - {attention} need attention"));
                                }
                                let t = ui
                                    .selectable_label(false, RichText::new(truncate(&text, 48)).strong())
                                    .on_hover_text(format!("{full}\nClick to {}", if *collapsed { "expand" } else { "collapse" }));
                                if b.clicked() || t.clicked() {
                                    toggle = Some(key.clone());
                                }
                                for _ in 0..8 {
                                    ui.label("");
                                }
                                ui.end_row();
                                continue;
                            }
                            Row::Repo(p) => p,
                        };
                        match self.infos.get(p) {
                            None => {
                                ui.label("…");
                                ui.label(p.to_string_lossy());
                                ui.end_row();
                            }
                            Some(i) => {
                                // draw the dot ourselves: the "●" glyph is missing from egui's default font
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                                ui.painter().circle_filled(rect.center(), 5.0, status_color(i));
                                let sel = self.selected.as_ref() == Some(p);
                                if ui.selectable_label(sel, &i.name).on_hover_text(p.to_string_lossy()).clicked() {
                                    clicked = Some(p.clone());
                                }
                                if let Some(e) = &i.error {
                                    ui.colored_label(RED, "error");
                                    ui.label(RichText::new(e).small().color(RED));
                                    ui.end_row();
                                    continue;
                                }
                                ui.label(&i.branch);
                                let sync = if i.upstream.is_none() {
                                    RichText::new("no upstream").weak()
                                } else if i.ahead == 0 && i.behind == 0 {
                                    RichText::new("in sync").color(GREEN)
                                } else {
                                    RichText::new(sync_text(i.ahead, i.behind)).color(BLUE)
                                };
                                let r = ui.label(sync);
                                if i.upstream.is_none() {
                                    r.on_hover_text(
                                        "This branch isn't linked to a remote branch, so there's nothing to compare with. \
                                         Usually a local-only repo or a branch that was never pushed.",
                                    );
                                } else if i.ahead > 0 || i.behind > 0 {
                                    r.on_hover_text("ahead = local commits not pushed yet\nbehind = remote commits you haven't pulled (as of last fetch)");
                                }
                                if i.is_dirty() {
                                    let mut t = format!("S{} M{} ?{}", i.staged, i.modified, i.untracked);
                                    if i.conflicted > 0 {
                                        t.push_str(&format!(" !{}", i.conflicted));
                                    }
                                    ui.label(RichText::new(t).color(if i.conflicted > 0 { RED } else { YELLOW }));
                                } else {
                                    ui.label(RichText::new("clean").color(GREEN));
                                }
                                ui.label(if i.stashes > 0 { i.stashes.to_string() } else { "-".into() });
                                match i.main_remote() {
                                    Some((name, url)) => {
                                        let web = git::web_url(url);
                                        let text = RichText::new(truncate(&git::short_remote(url), 42)).weak();
                                        let r = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                                        let hint = if web.is_some() { "\nClick to open in browser" } else { "" };
                                        let r = r.on_hover_text(format!("{name}: {url}{hint}"));
                                        if r.clicked() {
                                            if let Some(w) = web {
                                                ctx.open_url(egui::OpenUrl::new_tab(w));
                                            }
                                        }
                                    }
                                    None => {
                                        ui.label(RichText::new("no remote").weak());
                                    }
                                }
                                let msg: String = i.last_commit_msg.chars().take(60).collect();
                                ui.label(msg);
                                ui.label(i.last_commit_ts.map(git::ago).unwrap_or_else(|| "-".into()));
                                if ui.small_button("Open").on_hover_text("Open this folder in your file manager").clicked() {
                                    open_req = Some(p.clone());
                                }
                                ui.end_row();
                            }
                        }
                    }
                });
            });
            if let Some(c) = clicked {
                self.selected = if self.selected.as_ref() == Some(&c) { None } else { Some(c) };
            }
            if let Some(k) = toggle {
                if let Some(pos) = self.cfg.collapsed.iter().position(|x| x == &k) {
                    self.cfg.collapsed.remove(pos);
                } else {
                    self.cfg.collapsed.push(k);
                }
                cfg_changed = true;
            }
        });

        if let Some(p) = open_req {
            self.msg = match open_folder(&p) {
                Ok(()) => format!("Opened {}", p.display()),
                Err(e) => e,
            };
        }
        if let Some(p) = remove {
            self.cfg.repos_mut().retain(|r| r != &p);
            self.infos.remove(&p);
            self.selected = None;
            cfg_changed = true;
        }
        if cfg_changed {
            self.save();
        }
        if want_refresh {
            self.refresh(ctx);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 650.0])
            .with_title("RepoWatch"),
        ..Default::default()
    };
    eframe::run_native("RepoWatch", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
