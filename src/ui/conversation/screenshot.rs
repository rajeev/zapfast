//! The window while screenshots of a chat are taken: each page laid out at a
//! phone's width with the transcript's own bubbles, drawn alone in the window
//! and cut out of the window's picture. Nothing else is drawn meanwhile, so
//! the bubbles' remembered places and click targets belong to no other view.

use std::collections::HashMap;

use egui::{Align, Layout, Rect, Sense, UiBuilder, pos2, vec2};

use super::{
    MESSAGES_SIDE_MARGIN, RUN_GAP, RunPlace, View, WHATSAPP_SIDE_MARGIN, album_continues,
    album_run, bubble, bubble_id, last_bubble_of_run, last_of_run, same_run, shows_sender_pictures,
};
use crate::app::App;
use crate::model::Action;
use crate::screenshot::{self, Capture, Due, Phase, Shot, Span};
use crate::theme;
use crate::ui::widgets;

/// Whether the capture screen takes the window this frame: while pages are
/// laid out, taken, or written, not once the dialog offers them.
pub(crate) fn active(app: &App) -> bool {
    app.screenshot
        .as_ref()
        .is_some_and(|capture| !matches!(capture.phase, Phase::Done { .. }))
}

/// Draws the capture screen and moves the capture on by one frame.
pub(crate) fn show(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    // Repaints are event-driven, and nothing else asks for the frames a
    // capture needs.
    ctx.request_repaint();
    let Some(mut capture) = app.screenshot.take() else {
        return;
    };
    let outcome = step(app, ui, &mut capture);
    app.screenshot = Some(capture);
    if let Some(action) = outcome {
        app.actions.push(action);
    }
}

fn step(app: &mut App, ui: &mut egui::Ui, capture: &mut Capture) -> Option<Action> {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let palette = app.palette;
    ui.painter().rect_filled(screen, 0.0, palette.window);

    // The capture belongs to the chat on screen in the account on screen.
    if app.account().id != capture.account || app.open_chat.as_ref() != Some(&capture.chat) {
        return Some(Action::CancelScreenshot);
    }
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        return Some(Action::CancelScreenshot);
    }

    if let Phase::Saving(receiver) = &capture.phase {
        status(app, ui, screen, None);
        return match receiver.try_recv() {
            Ok(Ok(saved)) => {
                let thumbnails = saved
                    .thumbnails
                    .into_iter()
                    .enumerate()
                    .map(|(index, image)| {
                        ctx.load_texture(
                            format!("screenshot-{}-{index}", capture.generation),
                            image,
                            egui::TextureOptions::LINEAR,
                        )
                    })
                    .collect();
                // The files keep their names from one capture to the next,
                // so egui's cache would show the last capture's pages full
                // size.
                for path in &saved.paths {
                    crate::image_cache::forget(&ctx, &crate::util::image_uri(path));
                }
                capture.phase = Phase::Done {
                    paths: saved.paths,
                    thumbnails,
                };
                Some(Action::ScreenshotsTaken)
            }
            Ok(Err(error)) => Some(Action::ScreenshotFailed(error)),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Action::ScreenshotFailed("the page writer stopped".into()))
            }
        };
    }

    // The page's answer from the frame before, before this frame draws the
    // next one.
    let ppp = ctx.pixels_per_point();
    let answer = ctx.input(|input| {
        input.events.iter().find_map(|event| match event {
            egui::Event::Screenshot {
                user_data, image, ..
            } => user_data
                .data
                .as_ref()
                .and_then(|data| data.downcast_ref::<Shot>())
                .map(|shot| (*shot, image.clone())),
            _ => None,
        })
    });
    if let Some((shot, image)) = answer
        && let Some((_, page)) = capture.current_page()
    {
        let rect = page_rect(capture, screen, ppp, page);
        if capture.shot(shot, screenshot::crop(&image, rect, ppp)) {
            capture.save(screenshot::folder(&app.account().dirs.cache));
            return None;
        }
    }

    if let Phase::Measuring { frames: 0, .. } = capture.phase {
        // Chosen once: a different width would lay the bubbles out again.
        capture.page = vec2(
            screenshot::PAGE_WIDTH.min(screen.width() - 32.0),
            screenshot::PAGE_HEIGHT.min(screen.height() - screenshot::STATUS_HEIGHT - 16.0),
        );
        if capture.content_height() < 120.0 || capture.page.x < 200.0 {
            return Some(Action::ScreenshotFailed("the window is too small".into()));
        }
    }

    let page = capture.current_page();
    let span = page.map_or(
        Span {
            top: 0.0,
            bottom: capture.content_height(),
        },
        |(_, span)| span,
    );
    let rect = page_rect(capture, screen, ppp, span);
    let Some(rows) = draw_page(app, ui, capture, rect, span, page.map(|(index, _)| index)) else {
        return Some(Action::CancelScreenshot);
    };
    status(app, ui, screen, page.map(|(index, _)| index));

    match capture.phase {
        Phase::Measuring { .. } => {
            if capture.measured(rows) && capture.pages.is_empty() {
                return Some(Action::ScreenshotFailed("nothing to take".into()));
            }
        }
        Phase::Page { .. } => match capture.page_drawn(std::time::Instant::now()) {
            Due::Ask(shot) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(shot)));
            }
            Due::Wait => {}
            Due::GiveUp => {
                return Some(Action::ScreenshotFailed(
                    "the window did not answer for its picture".into(),
                ));
            }
        },
        Phase::Saving(_) | Phase::Done { .. } => {}
    }
    None
}

/// Where a page holding `span` of the column sits on screen: centred under
/// the status strip, on whole pixels so its picture cuts out cleanly.
fn page_rect(capture: &Capture, screen: Rect, ppp: f32, span: Span) -> Rect {
    let snap = |value: f32| (value * ppp).round() / ppp;
    let height = screenshot::HEADER_HEIGHT + 2.0 * screenshot::PAGE_PADDING + span.height();
    Rect::from_min_size(
        pos2(
            snap(screen.center().x - capture.page.x / 2.0),
            snap(screen.top() + screenshot::STATUS_HEIGHT),
        ),
        vec2(snap(capture.page.x), snap(height)),
    )
}

/// The strip above the page: how far the capture has got, and Cancel. It is
/// drawn on a layer over the page, which also keeps the pointer from
/// hovering the bubbles (a hovered bubble shows its menu button).
fn status(app: &mut App, ui: &egui::Ui, screen: Rect, page: Option<usize>) {
    let palette = app.palette;
    let locale = app.locale;
    let total = app
        .screenshot
        .as_ref()
        .map_or(0, |capture| capture.pages.len());
    let saving = app
        .screenshot
        .as_ref()
        .is_some_and(|capture| matches!(capture.phase, Phase::Saving(_)));
    let label = match page {
        Some(index) => crate::i18n::gettext(locale, "Taking screenshot {page} of {count}…")
            .replace("{page}", &(index + 1).to_string())
            .replace("{count}", &total.to_string()),
        None if saving => crate::i18n::gettext(locale, "Saving screenshots…").into_owned(),
        None => crate::i18n::gettext(locale, "Preparing screenshots…").into_owned(),
    };
    egui::Area::new(egui::Id::new("screenshot-status"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui.ctx(), |ui| {
            // Takes the pointer over the whole window.
            ui.interact(
                screen,
                egui::Id::new("screenshot-shield"),
                Sense::click_and_drag(),
            );
            let strip =
                Rect::from_min_size(screen.min, vec2(screen.width(), screenshot::STATUS_HEIGHT));
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(strip.shrink2(vec2(16.0, 0.0)))
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| {
                    if theme::pill_button(
                        ui,
                        &palette,
                        &crate::i18n::gettext(locale, "Cancel"),
                        false,
                    )
                    .clicked()
                    {
                        app.actions.push(Action::CancelScreenshot);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_space(theme::traffic_light_inset(ui.ctx()));
                        crate::motion::egui_spinner(ui);
                        theme::text(ui, &label, theme::regular(13.5), palette.secondary);
                    });
                },
            );
        });
}

/// Lays out the captured rows in a column at the page's width, shows the
/// stretch `span` of it in `rect` under the page's header, and returns
/// where each row lies in the column. `None` when the messages are gone.
fn draw_page(
    app: &mut App,
    ui: &mut egui::Ui,
    capture: &Capture,
    rect: Rect,
    span: Span,
    page: Option<usize>,
) -> Option<Vec<Span>> {
    let palette = app.palette;
    let (first, last, senders) = {
        let messages = &app.conversations.get(&capture.chat)?.messages;
        let first = messages
            .iter()
            .position(|message| message.id == capture.from)?;
        let last = messages
            .iter()
            .position(|message| message.id == capture.to)?;
        if last < first {
            return None;
        }
        let mut senders: Vec<String> = messages[first..=last]
            .iter()
            .map(|message| message.sender.clone())
            .collect();
        senders.sort();
        senders.dedup();
        (first, last, senders)
    };
    let mut avatars = HashMap::new();
    for sender in senders {
        let picture = app.avatar(&sender);
        avatars.insert(sender, picture);
    }
    let app = &*app;
    let chat = app.chat(&capture.chat)?;
    let messages = &app.conversations.get(&capture.chat)?.messages;

    // The page, opaque, as the chat looks: its background, and a header
    // naming the chat and the page.
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, palette.chat);
    let header = Rect::from_min_size(rect.min, vec2(rect.width(), screenshot::HEADER_HEIGHT));
    painter.rect_filled(header, 0.0, palette.panel);
    let title = widgets::line(
        ui,
        &app.chat_title(chat),
        theme::semibold(14.5),
        palette.text,
        header.width() - 80.0,
        1,
    );
    title.paint(
        ui,
        pos2(
            header.left() + 14.0,
            header.center().y - title.size().y / 2.0,
        ),
        palette.text,
    );
    if let Some(index) = page {
        let number = format!("{}/{}", index + 1, capture.pages.len());
        painter.text(
            pos2(header.right() - 14.0, header.center().y),
            egui::Align2::RIGHT_CENTER,
            number,
            theme::regular(12.5),
            palette.secondary,
        );
    }

    let side = f32::from(if palette.bubbles == theme::BubbleStyle::Messages {
        MESSAGES_SIDE_MARGIN
    } else {
        WHATSAPP_SIDE_MARGIN
    }) / 1.5;
    let content = Rect::from_min_max(
        pos2(rect.left(), header.bottom() + screenshot::PAGE_PADDING),
        pos2(rect.right(), rect.bottom() - screenshot::PAGE_PADDING),
    );
    let column_top = content.top() - span.top;
    let column = Rect::from_min_max(
        pos2(rect.left() + side, column_top),
        pos2(rect.right() - side, f32::INFINITY),
    );

    let names_or = |id: &str, hint: Option<&str>| app.display_name_or(id, hint);
    let mention_names = |id: &str| app.mention_name(id);
    let keyboard_navigation = std::cell::Cell::new(false);
    let view = View {
        palette,
        locale: app.locale,
        chat,
        me: app.me.as_deref(),
        auto_download: app.account().settings.auto_download,
        connected: app.link.is_connected(),
        account_receipts_off: app.account_receipts_off,
        poll_voting: &app.poll_voting,
        interactive_pending: &app.interactive_sending,
        // Nothing selects, scrolls, or opens a menu on a page.
        selecting: false,
        anchor: None,
        open_menu: None,
        starred: app.stars.get(&chat.id),
        pins: app.pins.get(&chat.id),
        pin_notices: &[],
        reaction: None,
        reaction_menu: false,
        reaction_emoji: &app.settings.reaction_emoji,
        keyboard_navigation: &keyboard_navigation,
        names_or: &names_or,
        mention_names: &mention_names,
        avatars: &avatars,
        contacts: &app.contacts,
        now: crate::util::now(),
        // A picture holds still: stickers and GIFs show their first frame.
        animate: false,
        motion_playing: None,
        player: &app.player,
        voice_speed: app.voice_speed_in(Some(&chat.id)),
        video: &app.video,
        // Not a copy source.
        copy_rows: None,
        download_progress: None,
    };

    // Whatever the bubbles ask for (a download, a reply on double click) is
    // not the reader's doing.
    let mut discarded = Vec::new();
    let mut rows = Vec::new();
    let mut column_ui = ui.new_child(
        UiBuilder::new()
            .id_salt(("screenshot-column", capture.generation))
            .max_rect(column)
            .layout(Layout::top_down(Align::Min)),
    );
    column_ui.set_clip_rect(content);
    column_ui.set_width(column.width());
    column_ui.spacing_mut().item_spacing.y = 3.0;
    let mut previous: Option<&crate::model::Message> = None;
    for index in first..=last {
        let message = &messages[index];
        if index != first && album_continues(messages, index) {
            previous = Some(message);
            continue;
        }
        let album = album_run(messages, index);
        let new_day = previous.is_none_or(|previous| {
            crate::util::day_key(previous.timestamp) != crate::util::day_key(message.timestamp)
        });
        let first_in_run = new_day || previous.is_none_or(|previous| !same_run(previous, message));
        let show_sender = shows_sender_pictures(chat) && !message.from_me && first_in_run;
        let show_avatar = if palette.bubbles == theme::BubbleStyle::Messages {
            shows_sender_pictures(chat) && !message.from_me && last_of_run(messages, index)
        } else {
            show_sender
        };
        let tailed = if palette.bubbles == theme::BubbleStyle::Messages {
            last_bubble_of_run(&palette, messages, index)
        } else {
            first_in_run
        };
        let row = column_ui.scope(|ui| {
            if new_day {
                ui.add_space(4.0);
                ui.vertical_centered(|ui| {
                    widgets::chip(
                        ui,
                        &palette,
                        &crate::util::day_label(app.locale, message.timestamp),
                    );
                });
                ui.add_space(4.0);
            } else if first_in_run {
                ui.add_space(RUN_GAP);
            }
            bubble(
                ui,
                &view,
                message,
                album,
                RunPlace {
                    show_sender,
                    show_avatar,
                    tailed,
                },
                &mut discarded,
            );
        });
        rows.push(Span {
            top: row.response.rect.top() - column_top,
            bottom: row.response.rect.bottom() - column_top,
        });
        previous = Some(message);
    }
    // The transcript keeps each bubble's place for its click targets, and
    // these places are on the page, not in the transcript.
    ui.ctx().data_mut(|data| {
        for message in &messages[first..=last] {
            let id = bubble_id(&chat.id, &message.id);
            data.remove::<Rect>(id.with("rect"));
            data.remove::<Rect>(id.with("body"));
            data.remove::<Rect>(id.with("menu-rect"));
        }
    });
    Some(rows)
}
