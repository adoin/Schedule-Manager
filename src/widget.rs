use crate::{
    calendar::{event_occurs_on, lunar_label, occurrence_start},
    models::{CalendarEvent, Holiday},
    repository::LocalRepository,
};
use anyhow::{Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate};
use chrono_tz::Asia::Shanghai;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame,
    Layout, Margin, RichText, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf, time::Instant};

const WIDGET_WIDTH: f32 = 820.0;
const WIDGET_HEIGHT: f32 = 610.0;
const WINDOW_IDENTITY: &str = "Schedule Manager Desktop Widget";
const DEBUG_GEOMETRY: bool = false;
const GLASS_SURFACE: Color32 = Color32::from_rgba_premultiplied(6, 12, 25, 54);
const GLASS_INNER: Color32 = Color32::from_rgba_premultiplied(13, 13, 13, 13);
const GLASS_BORDER: Color32 = Color32::from_rgba_premultiplied(109, 112, 118, 118);
const GLASS_INK: Color32 = Color32::from_rgba_premultiplied(239, 241, 244, 244);
const GLASS_MUTED: Color32 = Color32::from_rgba_premultiplied(151, 156, 166, 172);
const GLASS_FAINT: Color32 = Color32::from_rgba_premultiplied(92, 95, 101, 105);
const GLASS_DIVIDER: Color32 = Color32::from_rgba_premultiplied(35, 36, 38, 38);
const GLASS_ACCENT: Color32 = Color32::from_rgb(115, 139, 246);
const GLASS_CORAL: Color32 = Color32::from_rgb(255, 135, 143);

#[derive(Clone, Copy)]
struct CalendarSurfaceStyle {
    width: f32,
    header_height: f32,
    row_height: f32,
    column_gap: f32,
    row_gap: f32,
    cell_radius: u8,
    day_font_size: f32,
    lunar_font_size: f32,
    event_font_size: f32,
    max_event_chars: usize,
}

impl CalendarSurfaceStyle {
    const DESKTOP_WIDGET: Self = Self {
        width: 530.0,
        header_height: 24.0,
        row_height: 74.0,
        column_gap: 6.0,
        row_gap: 6.0,
        cell_radius: 11,
        day_font_size: 14.0,
        lunar_font_size: 10.0,
        event_font_size: 10.0,
        max_event_chars: 7,
    };

    fn height(self) -> f32 {
        self.header_height + self.row_gap + self.row_height * 6.0 + self.row_gap * 5.0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WidgetConfig {
    x: Option<i32>,
    y: Option<i32>,
    locked: bool,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            locked: true,
        }
    }
}

#[derive(Serialize)]
struct WidgetCommand<'a> {
    action: &'a str,
    event_id: Option<&'a str>,
    created_at: i64,
}

pub fn run() -> Result<()> {
    #[cfg(target_os = "windows")]
    let _single_instance = match windows_native::single_instance()? {
        Some(guard) => guard,
        None => return Ok(()),
    };
    let config = load_config().unwrap_or_default();
    let mut viewport = ViewportBuilder::default()
        .with_title(WINDOW_IDENTITY)
        .with_decorations(false)
        .with_resizable(false)
        .with_taskbar(false)
        // Do not expose the temporary Win32 frame while Glow selects its pixel
        // format. The first app frame installs the final styles and HRGN, then
        // makes the window visible.
        .with_visible(!cfg!(target_os = "windows"))
        .with_window_level(egui::WindowLevel::AlwaysOnBottom)
        .with_inner_size([WIDGET_WIDTH, WIDGET_HEIGHT])
        .with_min_inner_size([WIDGET_WIDTH, WIDGET_HEIGHT])
        .with_max_inner_size([WIDGET_WIDTH, WIDGET_HEIGHT])
        .with_transparent(true);
    if let (Some(x), Some(y)) = (config.x, config.y) {
        viewport = viewport.with_position([x as f32, y as f32]);
    }
    let options = eframe::NativeOptions {
        viewport,
        // On this Windows 10 compositor WGPU advertises no usable swapchain
        // alpha mode and presents transparent pixels as black. Glow asks for
        // an alpha-capable OpenGL framebuffer, allowing the Composition
        // backdrop visual hosted below it to remain visible.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_IDENTITY,
        options,
        Box::new(move |cc| Ok(Box::new(CalendarWidgetApp::new(cc, config)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct CalendarWidgetApp {
    visible_month: NaiveDate,
    selected_date: NaiveDate,
    selected_event_id: Option<String>,
    events: Vec<CalendarEvent>,
    holidays: HashMap<NaiveDate, Holiday>,
    config: WidgetConfig,
    fonts_installed: bool,
    last_refresh: Instant,
    last_position: Option<(i32, i32)>,
    position_changed_at: Option<Instant>,
    logo_texture: Option<egui::TextureHandle>,
    #[cfg(target_os = "windows")]
    wallpaper_backdrop: WallpaperBackdrop,
    #[cfg(target_os = "windows")]
    initial_show_pending: bool,
    glass_initialized: bool,
    #[cfg(target_os = "windows")]
    native_region_size: Option<(u32, u32)>,
    #[cfg(target_os = "windows")]
    native_desktop_attach_attempted: bool,
    #[cfg(target_os = "windows")]
    native_desktop_attached: bool,
    #[cfg(target_os = "windows")]
    native_drag: Option<windows_native::DragOrigin>,
    #[cfg(target_os = "windows")]
    native_diagnostics_logged: bool,
}

impl CalendarWidgetApp {
    fn new(cc: &eframe::CreationContext<'_>, config: WidgetConfig) -> Self {
        let today = Local::now().date_naive();
        let logo_texture = load_logo_texture(&cc.egui_ctx);
        let mut app = Self {
            visible_month: today.with_day(1).unwrap_or(today),
            selected_date: today,
            selected_event_id: None,
            events: Vec::new(),
            holidays: HashMap::new(),
            config,
            fonts_installed: false,
            last_refresh: Instant::now(),
            last_position: None,
            position_changed_at: None,
            logo_texture,
            #[cfg(target_os = "windows")]
            wallpaper_backdrop: WallpaperBackdrop::start(),
            #[cfg(target_os = "windows")]
            initial_show_pending: true,
            glass_initialized: false,
            #[cfg(target_os = "windows")]
            native_region_size: None,
            #[cfg(target_os = "windows")]
            native_desktop_attach_attempted: false,
            #[cfg(target_os = "windows")]
            native_desktop_attached: false,
            #[cfg(target_os = "windows")]
            native_drag: None,
            #[cfg(target_os = "windows")]
            native_diagnostics_logged: false,
        };
        app.refresh_data();
        app
    }

    fn install_fonts(&mut self, ctx: &egui::Context) {
        if self.fonts_installed {
            return;
        }
        self.fonts_installed = true;
        let candidates = if cfg!(target_os = "windows") {
            vec![
                r"C:\Windows\Fonts\msyh.ttc",
                r"C:\Windows\Fonts\segoeui.ttf",
            ]
        } else if cfg!(target_os = "macos") {
            vec![
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/SFNS.ttf",
            ]
        } else {
            vec!["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"]
        };
        for path in candidates {
            let Ok(bytes) = fs::read(path) else { continue };
            let mut fonts = FontDefinitions::default();
            fonts
                .font_data
                .insert("schedule-widget".into(), FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "schedule-widget".into());
            ctx.set_fonts(fonts);
            break;
        }
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::TRANSPARENT;
        visuals.window_fill = Color32::TRANSPARENT;
        visuals.faint_bg_color = Color32::TRANSPARENT;
        visuals.override_text_color = Some(GLASS_INK);
        visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(104, 130, 238, 125);
        visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(205, 216, 255));
        ctx.set_visuals(visuals);
    }

    fn refresh_data(&mut self) {
        let Ok(repository) = LocalRepository::open() else {
            return;
        };
        if let Ok(events) = repository.active_events() {
            self.events = events;
        }
        let start = self.visible_month - Duration::days(7);
        let end = self.visible_month + Duration::days(48);
        if let Ok(holidays) = repository.holidays_between(&start.to_string(), &end.to_string()) {
            self.holidays = holidays
                .into_iter()
                .filter_map(|holiday| {
                    NaiveDate::parse_from_str(&holiday.date, "%Y-%m-%d")
                        .ok()
                        .map(|date| (date, holiday))
                })
                .collect();
        }
        self.last_refresh = Instant::now();
    }

    fn selected_events(&self) -> Vec<&CalendarEvent> {
        let holiday = self.holidays.get(&self.selected_date);
        self.events
            .iter()
            .filter(|event| event_occurs_on(event, self.selected_date, holiday))
            .collect()
    }

    fn events_on(&self, date: NaiveDate) -> Vec<&CalendarEvent> {
        let holiday = self.holidays.get(&date);
        self.events
            .iter()
            .filter(|event| event_occurs_on(event, date, holiday))
            .collect()
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
        ui.horizontal(|ui| {
            if let Some(texture) = &self.logo_texture {
                ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::splat(24.0)));
            } else {
                ui.label(
                    RichText::new("日")
                        .strong()
                        .size(18.0)
                        .color(Color32::from_rgb(91, 111, 218)),
                );
            }
            if soft_button(ui, "‹", [36.0, 36.0]).clicked() {
                self.visible_month = (self.visible_month - Duration::days(1))
                    .with_day(1)
                    .unwrap_or(self.visible_month);
                self.refresh_data();
            }
            if soft_button(ui, "›", [36.0, 36.0]).clicked() {
                let next = self.visible_month + Duration::days(35);
                self.visible_month = next.with_day(1).unwrap_or(next);
                self.refresh_data();
            }

            let title_width = ui.available_width() - 290.0;
            let (title_rect, title_response) = ui.allocate_exact_size(
                Vec2::new(title_width.max(150.0), 36.0),
                if self.config.locked {
                    egui::Sense::hover()
                } else {
                    egui::Sense::click_and_drag()
                },
            );
            ui.painter().text(
                title_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                format!(
                    "{}年 {}月",
                    self.visible_month.year(),
                    self.visible_month.month()
                ),
                FontId::proportional(22.0),
                GLASS_INK,
            );
            if !self.config.locked {
                ui.painter().text(
                    title_rect.right_center() - Vec2::new(8.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    "≡  拖动以移动",
                    FontId::proportional(11.0),
                    if title_response.hovered() {
                        GLASS_INK
                    } else {
                        GLASS_MUTED
                    },
                );
                #[cfg(target_os = "windows")]
                {
                    if title_response.drag_started() {
                        self.native_drag = windows_native::begin_drag(frame);
                    }
                    if title_response.drag_stopped() {
                        self.native_drag = None;
                    }
                }
                #[cfg(not(target_os = "windows"))]
                if title_response.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
            }

            if soft_button(ui, "今天", [58.0, 36.0]).clicked() {
                let today = Local::now().date_naive();
                self.visible_month = today.with_day(1).unwrap_or(today);
                self.selected_date = today;
                self.refresh_data();
            }
            let lock_text = if self.config.locked {
                "已锁定"
            } else {
                "可拖动"
            };
            if accent_button(ui, lock_text, [82.0, 36.0], !self.config.locked).clicked() {
                self.config.locked = !self.config.locked;
                #[cfg(target_os = "windows")]
                if self.config.locked {
                    self.native_drag = None;
                }
                let _ = save_config(&self.config);
            }
            if soft_button(ui, "打开主程序", [86.0, 36.0]).clicked() {
                request_main("open", None);
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            if soft_button(ui, "×", [36.0, 36.0]).clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }

    fn calendar(&mut self, ui: &mut egui::Ui, style: CalendarSurfaceStyle) {
        let first = self.visible_month;
        let start = first - Duration::days(first.weekday().num_days_from_monday() as i64);
        let total_size = Vec2::new(style.width, style.height());
        let (surface, _) = ui.allocate_exact_size(total_size, egui::Sense::hover());
        let painter = ui.painter_at(surface);
        let cell_width = (style.width - style.column_gap * 6.0).max(7.0) / 7.0;

        for (column, label) in ["一", "二", "三", "四", "五", "六", "日"]
            .iter()
            .enumerate()
        {
            let x =
                surface.left() + column as f32 * (cell_width + style.column_gap) + cell_width / 2.0;
            painter.text(
                egui::Pos2::new(x, surface.top() + style.header_height / 2.0),
                egui::Align2::CENTER_CENTER,
                *label,
                FontId::proportional(12.0),
                GLASS_MUTED,
            );
        }

        let grid_top = surface.top() + style.header_height + style.row_gap / 2.0;
        painter.line_segment(
            [
                egui::Pos2::new(surface.left(), grid_top),
                egui::Pos2::new(surface.right(), grid_top),
            ],
            Stroke::new(1.0, GLASS_DIVIDER),
        );

        for index in 0..42 {
            let row = index / 7;
            let column = index % 7;
            let date = start + Duration::days(index as i64);
            let selected = date == self.selected_date;
            let in_month = date.month() == self.visible_month.month();
            let holiday_kind = self
                .holidays
                .get(&date)
                .map(|holiday| if holiday.is_off_day { "休" } else { "班" });
            let event_title = self
                .events_on(date)
                .first()
                .map(|event| truncate_calendar_text(&event.title, style.max_event_chars));
            let min = egui::Pos2::new(
                surface.left() + column as f32 * (cell_width + style.column_gap),
                surface.top()
                    + style.header_height
                    + style.row_gap
                    + row as f32 * (style.row_height + style.row_gap),
            );
            let rect = egui::Rect::from_min_size(min, Vec2::new(cell_width, style.row_height));
            let response = ui.interact(
                rect,
                ui.id().with(("widget-calendar-day", date.to_string())),
                egui::Sense::click(),
            );
            if selected || response.hovered() {
                painter.rect(
                    rect.shrink(2.0),
                    CornerRadius::same(style.cell_radius),
                    if selected {
                        Color32::from_rgba_unmultiplied(98, 124, 231, 128)
                    } else {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 15)
                    },
                    if selected {
                        Stroke::new(1.2, Color32::from_rgba_unmultiplied(210, 220, 255, 205))
                    } else {
                        Stroke::NONE
                    },
                    egui::StrokeKind::Inside,
                );
            }
            let day_color = if in_month {
                if date.weekday().number_from_monday() >= 6 {
                    GLASS_CORAL
                } else {
                    GLASS_INK
                }
            } else {
                GLASS_FAINT
            };
            painter.text(
                rect.left_top() + Vec2::new(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                date.day().to_string(),
                FontId::proportional(style.day_font_size),
                day_color,
            );
            painter.text(
                rect.right_top() + Vec2::new(-7.0, 9.0),
                egui::Align2::RIGHT_TOP,
                lunar_label(date),
                FontId::proportional(style.lunar_font_size),
                if in_month { GLASS_MUTED } else { GLASS_FAINT },
            );
            if let Some(title) = event_title {
                painter.text(
                    rect.left_bottom() + Vec2::new(8.0, -9.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("• {title}"),
                    FontId::proportional(style.event_font_size),
                    if in_month { GLASS_INK } else { GLASS_FAINT },
                );
            }
            if let Some(kind) = holiday_kind {
                let center = rect.right_bottom() + Vec2::new(-10.0, -10.0);
                painter.circle_filled(
                    center,
                    7.0,
                    Color32::from_rgba_unmultiplied(255, 177, 183, 58),
                );
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    kind,
                    FontId::proportional(8.0),
                    GLASS_CORAL,
                );
            }
            if response.clicked() {
                self.selected_date = date;
                self.selected_event_id = None;
            }

            if column == 6 && row < 5 {
                let y = rect.bottom() + style.row_gap / 2.0;
                painter.line_segment(
                    [
                        egui::Pos2::new(surface.left(), y),
                        egui::Pos2::new(surface.right(), y),
                    ],
                    Stroke::new(1.0, GLASS_DIVIDER),
                );
            }
        }
    }

    fn details(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(
            RichText::new(format!(
                "{} · {}",
                self.selected_date.format("%m月%d日"),
                weekday_name(self.selected_date)
            ))
            .size(18.0)
            .strong()
            .color(GLASS_INK),
        );
        ui.add_space(7.0);
        let (divider, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter().rect_filled(divider, 0.0, GLASS_DIVIDER);
        ui.add_space(7.0);
        let events = self
            .selected_events()
            .into_iter()
            .map(|event| {
                let time = occurrence_start(event, self.selected_date)
                    .unwrap_or(event.start_at)
                    .with_timezone(&Shanghai);
                (
                    event.id.clone(),
                    event.title.clone(),
                    event.notes.clone(),
                    time,
                )
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("这一天还没有日程").color(GLASS_MUTED));
            });
        } else {
            for (id, title, notes, time) in events {
                let selected = self.selected_event_id.as_deref() == Some(id.as_str());
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 62.0),
                    egui::Sense::click(),
                );
                if selected || response.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(10),
                        if selected {
                            Color32::from_rgba_unmultiplied(104, 130, 238, 64)
                        } else {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 12)
                        },
                    );
                }
                let line_x = rect.left() + 4.0;
                ui.painter().line_segment(
                    [
                        egui::Pos2::new(line_x, rect.top() + 8.0),
                        egui::Pos2::new(line_x, rect.bottom() - 8.0),
                    ],
                    Stroke::new(2.0, GLASS_ACCENT),
                );
                ui.painter().circle_filled(
                    egui::Pos2::new(line_x, rect.top() + 16.0),
                    3.5,
                    GLASS_ACCENT,
                );
                ui.painter().text(
                    rect.left_top() + Vec2::new(14.0, 9.0),
                    egui::Align2::LEFT_TOP,
                    time.format("%H:%M").to_string(),
                    FontId::proportional(10.0),
                    GLASS_MUTED,
                );
                ui.painter().text(
                    rect.left_top() + Vec2::new(60.0, 7.0),
                    egui::Align2::LEFT_TOP,
                    truncate_calendar_text(&title, 10),
                    FontId::proportional(13.0),
                    GLASS_INK,
                );
                if !notes.is_empty() {
                    ui.painter().text(
                        rect.left_top() + Vec2::new(60.0, 29.0),
                        egui::Align2::LEFT_TOP,
                        truncate_calendar_text(&notes, 16),
                        FontId::proportional(10.0),
                        GLASS_MUTED,
                    );
                }
                if response.clicked() {
                    self.selected_event_id = Some(id);
                }
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if let Some(id) = self.selected_event_id.clone() {
                if accent_button(ui, "在主程序中编辑", [ui.available_width(), 38.0], true).clicked()
                {
                    request_main("edit", Some(&id));
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            } else {
                ui.label(
                    RichText::new("点击日程查看详情")
                        .size(11.0)
                        .color(GLASS_MUTED),
                );
            }
        });
    }
}

impl eframe::App for CalendarWidgetApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if take_exit_signal() {
            eprintln!("widget-exit-signal status=received");
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        self.install_fonts(ctx);
        #[cfg(target_os = "windows")]
        {
            if !self.native_desktop_attach_attempted {
                self.native_desktop_attach_attempted = true;
                match windows_native::attach_to_desktop(frame) {
                    Ok(()) => self.native_desktop_attached = true,
                    Err(error) => {
                        eprintln!("widget-desktop-attach status=failed error={error:#}")
                    }
                }
            }
            windows_native::maintain(
                frame,
                ctx,
                &mut self.native_region_size,
                self.native_desktop_attached,
            );
            self.wallpaper_backdrop.poll(ctx);
            if let Some(origin) = self.native_drag {
                if ctx.input(|input| input.pointer.primary_down()) {
                    if let Some(position) = windows_native::drag_to(frame, origin) {
                        self.last_position = Some(position);
                        self.position_changed_at = Some(Instant::now());
                        ctx.request_repaint();
                    }
                } else {
                    self.native_drag = None;
                    ctx.request_repaint();
                }
            }
            if self.initial_show_pending
                && self.wallpaper_backdrop.texture.is_some()
                && windows_native::ready(frame, self.native_desktop_attached)
            {
                self.initial_show_pending = false;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            }
        }
        if !self.glass_initialized {
            if let Err(error) = initialize_glass(frame) {
                eprintln!("desktop widget glass initialization failed: {error:#}");
            }
            self.glass_initialized = true;
        }
        #[cfg(target_os = "windows")]
        if !self.native_diagnostics_logged {
            eprintln!("{}", windows_native::diagnostic_snapshot(frame, ctx));
            self.native_diagnostics_logged = true;
        }

        if self.last_refresh.elapsed().as_secs() >= 30 {
            self.refresh_data();
        }
        if let Some(position) = native_window_position(frame) {
            if self.last_position != Some(position) {
                self.last_position = Some(position);
                self.position_changed_at = Some(Instant::now());
            }
        }
        if self
            .position_changed_at
            .is_some_and(|changed| changed.elapsed().as_millis() >= 350)
            && let Some((x, y)) = self.last_position
        {
            self.config.x = Some(x);
            self.config.y = Some(y);
            let _ = save_config(&self.config);
            self.position_changed_at = None;
        }
        #[cfg(target_os = "windows")]
        let repaint_interval_ms = if self.native_drag.is_some() { 8 } else { 16 };
        #[cfg(not(target_os = "windows"))]
        let repaint_interval_ms = 16;
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint_interval_ms));
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        let window_rect = root_ui.max_rect();
        #[cfg(target_os = "windows")]
        if self.native_drag.is_none()
            && let (Some(texture), Some(uv)) = (
                self.wallpaper_backdrop.texture.as_ref(),
                windows_native::wallpaper_uv(frame),
            )
        {
            root_ui
                .painter()
                .image(texture.id(), window_rect, uv, Color32::WHITE);
        }
        let outer_radius = if cfg!(target_os = "windows") {
            CornerRadius::ZERO
        } else {
            CornerRadius::same(24)
        };
        let outer_margin = if cfg!(target_os = "windows") { 0 } else { 8 };
        let inner_margin = if cfg!(target_os = "windows") { 22 } else { 14 };
        let outer_stroke = if cfg!(target_os = "windows") {
            Stroke::NONE
        } else {
            Stroke::new(1.0, GLASS_BORDER)
        };
        Frame::new()
            .fill(GLASS_SURFACE)
            .stroke(outer_stroke)
            .corner_radius(outer_radius)
            .outer_margin(Margin::same(outer_margin))
            .inner_margin(Margin::same(inner_margin))
            .show(root_ui, |ui| {
                ui.set_min_size(Vec2::new(776.0, 566.0));
                ui.spacing_mut().item_spacing.y = 0.0;
                self.header(ui, &ctx, frame);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.allocate_ui_with_layout(
                        Vec2::new(550.0, 518.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.add_space(10.0);
                                self.calendar(ui, CalendarSurfaceStyle::DESKTOP_WIDGET);
                            });
                        },
                    );
                    ui.add_space(12.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(214.0, 518.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            Frame::new()
                                .fill(GLASS_INNER)
                                .stroke(Stroke::new(1.0, GLASS_DIVIDER))
                                .corner_radius(CornerRadius::same(16))
                                .inner_margin(Margin::same(12))
                                .show(ui, |ui| {
                                    ui.set_min_size(Vec2::new(190.0, 494.0));
                                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                                        self.details(ui, &ctx);
                                    });
                                });
                        },
                    );
                });
            });
        if DEBUG_GEOMETRY {
            let painter = root_ui.painter();
            painter.rect_stroke(
                window_rect.shrink(0.5),
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::from_rgb(255, 0, 190)),
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                window_rect.shrink(4.0),
                CornerRadius::same(24),
                Stroke::new(1.0, Color32::from_rgb(0, 235, 255)),
                egui::StrokeKind::Inside,
            );
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn soft_button(ui: &mut egui::Ui, text: &str, size: [f32; 2]) -> egui::Response {
    ui.add_sized(
        size,
        egui::Button::new(RichText::new(text).strong().color(GLASS_INK))
            .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 18))
            .stroke(Stroke::new(1.0, GLASS_DIVIDER))
            .corner_radius(10),
    )
}

fn accent_button(ui: &mut egui::Ui, text: &str, size: [f32; 2], accent: bool) -> egui::Response {
    ui.add_sized(
        size,
        egui::Button::new(RichText::new(text).strong().color(if accent {
            Color32::WHITE
        } else {
            GLASS_INK
        }))
        .fill(if accent {
            Color32::from_rgba_unmultiplied(99, 122, 226, 205)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 18)
        })
        .stroke(Stroke::new(1.0, GLASS_DIVIDER))
        .corner_radius(10),
    )
}

#[cfg(target_os = "windows")]
fn initialize_glass(_frame: &eframe::Frame) -> Result<()> {
    // Windows paints the pre-blurred wallpaper sample inside the same Glow
    // surface. No second HWND or system backdrop is required.
    Ok(())
}

#[cfg(target_os = "macos")]
fn initialize_glass(frame: &eframe::Frame) -> Result<()> {
    window_vibrancy::apply_vibrancy(
        frame,
        window_vibrancy::NSVisualEffectMaterial::HudWindow,
        Some(window_vibrancy::NSVisualEffectState::Active),
        Some(24.0),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn initialize_glass(_frame: &eframe::Frame) -> Result<()> {
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn load_logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(include_bytes!("../assets/schedule-logo-64.png"))
        .ok()?
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(
        "schedule-widget-logo",
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

#[cfg(target_os = "windows")]
struct WallpaperBackdrop {
    texture: Option<egui::TextureHandle>,
    receiver: Option<std::sync::mpsc::Receiver<Result<([usize; 2], Vec<u8>)>>>,
}

#[cfg(target_os = "windows")]
impl WallpaperBackdrop {
    fn start() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("widget-wallpaper-blur".into())
            .spawn(move || {
                let _ = sender.send(load_blurred_wallpaper());
            });
        Self {
            texture: None,
            receiver: Some(receiver),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok((size, pixels))) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                self.texture = Some(ctx.load_texture(
                    "schedule-widget-wallpaper-blur",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
                self.receiver = None;
                eprintln!(
                    "widget-wallpaper-backdrop status=ready texture={}x{} renderer=glow",
                    size[0], size[1]
                );
            }
            Ok(Err(error)) => {
                self.receiver = None;
                eprintln!("widget-wallpaper-backdrop status=failed error={error:#}");
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                eprintln!("widget-wallpaper-backdrop status=failed error=worker disconnected");
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn load_blurred_wallpaper() -> Result<([usize; 2], Vec<u8>)> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETDESKWALLPAPER, SystemParametersInfoW,
    };

    let mut path = vec![0_u16; 32_768];
    let loaded = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            path.len() as u32,
            path.as_mut_ptr() as _,
            0,
        )
    };
    if loaded == 0 {
        anyhow::bail!("SystemParametersInfoW(SPI_GETDESKWALLPAPER) failed");
    }
    let length = path
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(path.len());
    let path = PathBuf::from(OsString::from_wide(&path[..length]));
    let source = image::open(&path)
        .with_context(|| format!("cannot decode wallpaper {}", path.display()))?
        .into_rgba8();

    // The texture only needs enough resolution for a strongly blurred sample.
    // Keeping it at 960×540 makes startup inexpensive while preserving the
    // wallpaper's large shapes and color transitions.
    let resized = image::imageops::resize(&source, 960, 540, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::blur(&resized, 10.0);
    Ok((
        [blurred.width() as usize, blurred.height() as usize],
        blurred.into_raw(),
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn load_logo_texture(_ctx: &egui::Context) -> Option<egui::TextureHandle> {
    None
}

fn truncate_calendar_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn weekday_name(date: NaiveDate) -> &'static str {
    match date.weekday().num_days_from_monday() {
        0 => "周一",
        1 => "周二",
        2 => "周三",
        3 => "周四",
        4 => "周五",
        5 => "周六",
        _ => "周日",
    }
}

fn config_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")
        .context("cannot resolve widget config directory")?;
    Ok(project.config_dir().join("desktop-widget.json"))
}

fn command_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")
        .context("cannot resolve widget command directory")?;
    Ok(project.data_local_dir().join("widget-command.json"))
}

fn exit_signal_path() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")
        .context("cannot resolve widget command directory")?;
    Ok(project.data_local_dir().join("desktop-widget-exit.signal"))
}

fn take_exit_signal() -> bool {
    let Ok(path) = exit_signal_path() else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    match fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            eprintln!(
                "widget-exit-signal status=failed path={} error={error}",
                path.display()
            );
            false
        }
    }
}

fn load_config() -> Result<WidgetConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(WidgetConfig::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn save_config(config: &WidgetConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

fn request_main(action: &str, event_id: Option<&str>) {
    let Ok(path) = command_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let command = WidgetCommand {
        action,
        event_id,
        created_at: chrono::Utc::now().timestamp_millis(),
    };
    if let Ok(payload) = serde_json::to_vec(&command) {
        let _ = fs::write(path, payload);
    }
}

#[cfg(target_os = "windows")]
fn native_window_position(frame: &eframe::Frame) -> Option<(i32, i32)> {
    windows_native::position(frame)
}

#[cfg(not(target_os = "windows"))]
fn native_window_position(_frame: &eframe::Frame) -> Option<(i32, i32)> {
    None
}

#[cfg(target_os = "windows")]
mod windows_native {
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        ffi::OsStr,
        os::windows::ffi::OsStrExt,
        sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT,
            RECT, WPARAM,
        },
        Graphics::{
            Dwm::{
                DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_EXTENDED_FRAME_BOUNDS,
                DWMWA_NCRENDERING_POLICY, DWMWA_VISIBLE_FRAME_BORDER_THICKNESS,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmGetWindowAttribute,
                DwmSetWindowAttribute,
            },
            Gdi::{
                CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, GetWindowRgnBox,
                MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, RGN_ERROR, SetWindowRgn,
            },
        },
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{
            CallWindowProcW, DefWindowProcW, EnumWindows, FindWindowExW, FindWindowW, GWL_EXSTYLE,
            GWL_STYLE, GWLP_WNDPROC, GetClientRect, GetCursorPos, GetParent, GetWindowLongW,
            GetWindowRect, HWND_TOP, SMTO_NORMAL, STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE,
            SWP_NOCOPYBITS, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageTimeoutW, SetParent,
            SetWindowLongPtrW, SetWindowLongW, SetWindowPos, WM_NCACTIVATE, WM_NCCALCSIZE,
            WM_NCPAINT, WM_STYLECHANGING, WNDPROC, WS_BORDER, WS_CHILD, WS_DLGFRAME,
            WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE,
            WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
        },
    };
    use windows_sys::core::BOOL;

    static ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);
    static DESKTOP_CHILD: AtomicBool = AtomicBool::new(false);

    pub struct SingleInstance(HANDLE);

    #[derive(Clone, Copy)]
    pub struct DragOrigin {
        cursor: POINT,
        window_x: i32,
        window_y: i32,
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn single_instance() -> anyhow::Result<Option<SingleInstance>> {
        let name = wide("Local\\ScheduleManagerDesktopWidget");
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(anyhow::anyhow!("CreateMutexW failed"));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(SingleInstance(handle)))
    }

    fn hwnd(frame: &eframe::Frame) -> Option<HWND> {
        let handle = frame.window_handle().ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as _),
            _ => None,
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    unsafe extern "system" fn frameless_wndproc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCCALCSIZE | WM_NCPAINT => return 0,
            WM_NCACTIVATE => return 1,
            WM_STYLECHANGING if lparam != 0 => unsafe {
                let style = &mut *(lparam as *mut STYLESTRUCT);
                if wparam as i32 == GWL_STYLE {
                    let frame = WS_CHILD
                        | WS_POPUP
                        | WS_THICKFRAME
                        | WS_BORDER
                        | WS_DLGFRAME
                        | WS_MINIMIZEBOX
                        | WS_MAXIMIZEBOX
                        | WS_SYSMENU;
                    style.styleNew = (style.styleNew & !frame)
                        | if DESKTOP_CHILD.load(Ordering::Acquire) {
                            WS_CHILD
                        } else {
                            WS_POPUP
                        };
                } else if wparam as i32 == GWL_EXSTYLE {
                    let frame = WS_EX_DLGMODALFRAME
                        | WS_EX_WINDOWEDGE
                        | WS_EX_CLIENTEDGE
                        | WS_EX_STATICEDGE;
                    style.styleNew &= !frame;
                }
            },
            _ => {}
        }
        let original = ORIGINAL_WNDPROC.load(Ordering::Acquire);
        if original == 0 {
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        } else {
            let procedure: WNDPROC = unsafe { std::mem::transmute(original) };
            unsafe { CallWindowProcW(procedure, window, message, wparam, lparam) }
        }
    }

    unsafe fn install_frameless_wndproc(window: HWND) {
        if ORIGINAL_WNDPROC.load(Ordering::Acquire) != 0 {
            return;
        }
        let previous = unsafe {
            SetWindowLongPtrW(
                window,
                GWLP_WNDPROC,
                frameless_wndproc as *const () as isize,
            )
        };
        if previous != 0 {
            ORIGINAL_WNDPROC.store(previous, Ordering::Release);
            eprintln!("widget-frameless-wndproc status=ready hwnd={window:p}");
        } else {
            eprintln!("widget-frameless-wndproc status=failed hwnd={window:p}");
        }
    }

    unsafe extern "system" fn find_interactive_desktop_host(top: HWND, data: LPARAM) -> BOOL {
        let shell_class = wide("SHELLDLL_DefView");
        unsafe {
            let shell = FindWindowExW(
                top,
                std::ptr::null_mut(),
                shell_class.as_ptr(),
                std::ptr::null(),
            );
            if !shell.is_null() {
                // Use the WorkerW/Progman that owns SHELLDLL_DefView rather
                // than the empty WorkerW behind it.  The latter is suitable
                // for non-interactive wallpapers, but the DefView sitting
                // above it consumes all mouse input before our child sees it.
                *(data as *mut HWND) = top;
                return 0;
            }
        }
        1
    }

    unsafe fn interactive_desktop_host() -> anyhow::Result<HWND> {
        let progman_class = wide("Progman");
        let progman = unsafe { FindWindowW(progman_class.as_ptr(), std::ptr::null()) };
        if progman.is_null() {
            anyhow::bail!("Progman window not found");
        }
        let mut message_result = 0usize;
        unsafe {
            SendMessageTimeoutW(
                progman,
                0x052C,
                0xDu32 as WPARAM,
                0,
                SMTO_NORMAL,
                1000,
                &mut message_result,
            );
            SendMessageTimeoutW(
                progman,
                0x052C,
                0xDu32 as WPARAM,
                1,
                SMTO_NORMAL,
                1000,
                &mut message_result,
            );
        }
        let mut worker: HWND = std::ptr::null_mut();
        unsafe {
            EnumWindows(
                Some(find_interactive_desktop_host),
                &mut worker as *mut HWND as LPARAM,
            );
        }
        Ok(if worker.is_null() { progman } else { worker })
    }

    pub fn attach_to_desktop(frame: &eframe::Frame) -> anyhow::Result<()> {
        let window = hwnd(frame).ok_or_else(|| anyhow::anyhow!("widget HWND unavailable"))?;
        unsafe {
            install_frameless_wndproc(window);
            let worker = interactive_desktop_host()?;
            let mut window_rect = RECT::default();
            let mut worker_rect = RECT::default();
            if GetWindowRect(window, &mut window_rect) == 0
                || GetWindowRect(worker, &mut worker_rect) == 0
            {
                anyhow::bail!("cannot read widget or WorkerW bounds");
            }

            DESKTOP_CHILD.store(true, Ordering::Release);
            suppress_chrome(window, true);
            SetParent(window, worker);
            if GetParent(window) != worker {
                DESKTOP_CHILD.store(false, Ordering::Release);
                suppress_chrome(window, false);
                anyhow::bail!("SetParent(WorkerW) did not persist");
            }
            let width = (window_rect.right - window_rect.left).max(1);
            let height = (window_rect.bottom - window_rect.top).max(1);
            let x = window_rect.left - worker_rect.left;
            let y = window_rect.top - worker_rect.top;
            if SetWindowPos(
                window,
                // Keep the widget above SHELLDLL_DefView so it remains
                // clickable, while both still live under the desktop host.
                HWND_TOP,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            ) == 0
            {
                // Do not leave the window in a half-attached state.  A failed
                // child positioning call must restore the original top-level
                // window role so the widget can still be used normally.
                SetParent(window, std::ptr::null_mut());
                DESKTOP_CHILD.store(false, Ordering::Release);
                suppress_chrome(window, false);
                SetWindowPos(
                    window,
                    std::ptr::null_mut(),
                    window_rect.left,
                    window_rect.top,
                    width,
                    height,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                anyhow::bail!("cannot position widget inside WorkerW");
            }
            eprintln!(
                "widget-desktop-attach status=ready hwnd={window:p} worker={worker:p} position={x},{y} size={width}x{height}"
            );
        }
        Ok(())
    }

    unsafe fn suppress_chrome(window: HWND, as_child: bool) {
        unsafe {
            let style = GetWindowLongW(window, GWL_STYLE) as u32;
            let frame_style = WS_CHILD
                | WS_POPUP
                | WS_THICKFRAME
                | WS_BORDER
                | WS_DLGFRAME
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX
                | WS_SYSMENU;
            let desired_style = (style & !frame_style) | if as_child { WS_CHILD } else { WS_POPUP };
            let ex_style = GetWindowLongW(window, GWL_EXSTYLE) as u32;
            let frame_ex =
                WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE;
            let desired_ex = ex_style & !frame_ex;
            let changed = style != desired_style || ex_style != desired_ex;
            if style != desired_style {
                SetWindowLongW(window, GWL_STYLE, desired_style as i32);
            }
            if ex_style != desired_ex {
                SetWindowLongW(window, GWL_EXSTYLE, desired_ex as i32);
            }
            if changed {
                SetWindowPos(
                    window,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }

            let nc = DWMNCRP_DISABLED;
            let _ = DwmSetWindowAttribute(
                window,
                DWMWA_NCRENDERING_POLICY as u32,
                &nc as *const i32 as _,
                std::mem::size_of_val(&nc) as u32,
            );
            let corner = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(
                window,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &corner as *const i32 as _,
                std::mem::size_of_val(&corner) as u32,
            );
            let border = 0xffff_fffeu32;
            let _ = DwmSetWindowAttribute(
                window,
                DWMWA_BORDER_COLOR as u32,
                &border as *const u32 as _,
                std::mem::size_of_val(&border) as u32,
            );
        }
    }

    pub fn maintain(
        frame: &eframe::Frame,
        ctx: &egui::Context,
        cached_size: &mut Option<(u32, u32)>,
        desktop_attached: bool,
    ) {
        let Some(window) = hwnd(frame) else {
            return;
        };
        unsafe {
            install_frameless_wndproc(window);
            DESKTOP_CHILD.store(desktop_attached, Ordering::Release);
            suppress_chrome(window, desktop_attached);
            if desktop_attached {
                SetWindowPos(
                    window,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }

            let mut client = RECT::default();
            if GetClientRect(window, &mut client) == 0 {
                return;
            }
            let size = (
                (client.right - client.left).max(0) as u32,
                (client.bottom - client.top).max(0) as u32,
            );
            let mut bounds = RECT::default();
            let region_kind = GetWindowRgnBox(window, &mut bounds);
            let region_was_replaced = region_kind == RGN_ERROR
                || bounds.left != 0
                || bounds.top != 1
                || bounds.right != size.0.saturating_sub(1) as i32
                || bounds.bottom != size.1.saturating_sub(1) as i32;
            if *cached_size != Some(size) || region_was_replaced {
                let diameter = (48.0 * ctx.pixels_per_point()).round().max(1.0) as i32;
                // HRGN is the final native boundary on Windows. Excluding y=0
                // also removes the unstable first swapchain scanline/top edge.
                let region =
                    CreateRoundRectRgn(0, 1, size.0 as i32, size.1 as i32, diameter, diameter);
                if !region.is_null() {
                    if SetWindowRgn(window, region, 1) == 0 {
                        DeleteObject(region);
                    } else {
                        *cached_size = Some(size);
                    }
                }
            }
        }
    }

    pub fn ready(frame: &eframe::Frame, desktop_attached: bool) -> bool {
        let Some(window) = hwnd(frame) else {
            return false;
        };
        unsafe {
            let style = GetWindowLongW(window, GWL_STYLE) as u32;
            let forbidden = WS_THICKFRAME
                | WS_BORDER
                | WS_DLGFRAME
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX
                | WS_SYSMENU;
            let mut bounds = RECT::default();
            let role_ready = if desktop_attached {
                style & WS_CHILD != 0 && !GetParent(window).is_null()
            } else {
                style & WS_POPUP != 0 && style & WS_CHILD == 0
            };
            role_ready
                && style & forbidden == 0
                && GetWindowRgnBox(window, &mut bounds) != RGN_ERROR
        }
    }

    pub fn wallpaper_uv(frame: &eframe::Frame) -> Option<egui::Rect> {
        let window = hwnd(frame)?;
        unsafe {
            let mut window_rect = RECT::default();
            if GetWindowRect(window, &mut window_rect) == 0 {
                return None;
            }
            let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() {
                return None;
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            let monitor_width = (info.rcMonitor.right - info.rcMonitor.left).max(1) as f32;
            let monitor_height = (info.rcMonitor.bottom - info.rcMonitor.top).max(1) as f32;
            let u0 = (window_rect.left - info.rcMonitor.left) as f32 / monitor_width;
            let v0 = (window_rect.top - info.rcMonitor.top) as f32 / monitor_height;
            let u1 = (window_rect.right - info.rcMonitor.left) as f32 / monitor_width;
            let v1 = (window_rect.bottom - info.rcMonitor.top) as f32 / monitor_height;
            Some(egui::Rect::from_min_max(
                egui::Pos2::new(u0, v0),
                egui::Pos2::new(u1, v1),
            ))
        }
    }

    pub fn position(frame: &eframe::Frame) -> Option<(i32, i32)> {
        let window = hwnd(frame)?;
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(window, &mut rect) } == 0 {
            None
        } else {
            Some((rect.left, rect.top))
        }
    }

    pub fn begin_drag(frame: &eframe::Frame) -> Option<DragOrigin> {
        let window = hwnd(frame)?;
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        unsafe {
            if GetCursorPos(&mut cursor) == 0 || GetWindowRect(window, &mut rect) == 0 {
                return None;
            }
        }
        Some(DragOrigin {
            cursor,
            window_x: rect.left,
            window_y: rect.top,
        })
    }

    pub fn drag_to(frame: &eframe::Frame, origin: DragOrigin) -> Option<(i32, i32)> {
        let window = hwnd(frame)?;
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) } == 0 {
            return None;
        }
        let x = origin.window_x + cursor.x - origin.cursor.x;
        let y = origin.window_y + cursor.y - origin.cursor.y;
        let moved = unsafe {
            let parent = GetParent(window);
            let z_order_flag = if parent.is_null() { SWP_NOZORDER } else { 0 };
            let (native_x, native_y) = if parent.is_null() {
                (x, y)
            } else {
                let mut parent_rect = RECT::default();
                if GetWindowRect(parent, &mut parent_rect) == 0 {
                    return None;
                }
                (x - parent_rect.left, y - parent_rect.top)
            };
            SetWindowPos(
                window,
                if parent.is_null() {
                    std::ptr::null_mut()
                } else {
                    HWND_TOP
                },
                native_x,
                native_y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | z_order_flag | SWP_NOCOPYBITS,
            )
        };
        if moved == 0 { None } else { Some((x, y)) }
    }

    pub fn diagnostic_snapshot(frame: &eframe::Frame, ctx: &egui::Context) -> String {
        let Some(window) = hwnd(frame) else {
            return "widget-native-diagnostic hwnd=unavailable".to_owned();
        };
        unsafe {
            let style = GetWindowLongW(window, GWL_STYLE) as u32;
            let ex_style = GetWindowLongW(window, GWL_EXSTYLE) as u32;
            let parent = GetParent(window);
            let mut window_rect = RECT::default();
            let mut client_rect = RECT::default();
            let mut extended_rect = RECT::default();
            let mut region_rect = RECT::default();
            let mut corner = 0i32;
            let mut visible_border = 0u32;
            let window_ok = GetWindowRect(window, &mut window_rect);
            let client_ok = GetClientRect(window, &mut client_rect);
            let extended_result = DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut extended_rect as *mut RECT as _,
                std::mem::size_of::<RECT>() as u32,
            );
            let corner_result = DwmGetWindowAttribute(
                window,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &mut corner as *mut i32 as _,
                std::mem::size_of::<i32>() as u32,
            );
            let border_result = DwmGetWindowAttribute(
                window,
                DWMWA_VISIBLE_FRAME_BORDER_THICKNESS as u32,
                &mut visible_border as *mut u32 as _,
                std::mem::size_of::<u32>() as u32,
            );
            let region_kind = GetWindowRgnBox(window, &mut region_rect);
            format!(
                "widget-native-diagnostic hwnd={window:p} parent={parent:p} ppp={:.3} style=0x{style:08x} ex_style=0x{ex_style:08x} window_ok={window_ok} window={},{},{},{} client_ok={client_ok} client={},{},{},{} extended_hr=0x{:08x} extended={},{},{},{} corner_hr=0x{:08x} corner={} border_hr=0x{:08x} border={} region_kind={} region={},{},{},{}",
                ctx.pixels_per_point(),
                window_rect.left,
                window_rect.top,
                window_rect.right,
                window_rect.bottom,
                client_rect.left,
                client_rect.top,
                client_rect.right,
                client_rect.bottom,
                extended_result as u32,
                extended_rect.left,
                extended_rect.top,
                extended_rect.right,
                extended_rect.bottom,
                corner_result as u32,
                corner,
                border_result as u32,
                visible_border,
                region_kind,
                region_rect.left,
                region_rect.top,
                region_rect.right,
                region_rect.bottom,
            )
        }
    }
}
