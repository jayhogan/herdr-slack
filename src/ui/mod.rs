//! Drawing the inbox pane (spec §9 "The TUI").

pub mod list;
pub mod theme;

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Span;

    use super::list;
    use crate::app::App;
    use crate::config::Config;
    use crate::fixtures::sample_cache;
    use crate::slack::SlackError;
    use crate::token;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_792_100_000, 0).unwrap()
    }

    fn app() -> App {
        let mut app = App::new(Config::default());
        app.set_cache(sample_cache(now()));
        app
    }

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), now());
    }

    fn draw(a: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let screen = a.screen(now());
        let mut scroll = 0;
        terminal
            .draw(|f| list::render(f, &screen, &mut scroll))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// One string per terminal line, skipping cells hidden behind wide characters.
    fn text_lines(buf: &Buffer) -> Vec<String> {
        let width = buf.area.width as usize;
        buf.content
            .chunks(width)
            .map(|row| {
                let mut out = String::new();
                let mut skip = 0;
                for c in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    out.push_str(c.symbol());
                    skip = Span::raw(c.symbol()).width().saturating_sub(1);
                }
                out.trim_end().to_string()
            })
            .collect()
    }

    fn text(buf: &Buffer) -> String {
        text_lines(buf).join("\n")
    }

    #[test]
    fn header_drops_sort_then_breakdown_then_synced() {
        let screen = app().screen(now());
        let h = |w: usize| {
            let line = list::header_line(&screen.header, w);
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let full = "Slack inbox │ 9 need attention · 2 watch · 3 mention · 2 reply · 2 thread · 2 channel · 1 dm   sort: oldest waiting │ synced 2 min ago";
        let w = full.chars().count();
        assert_eq!(h(w).trim_end(), full);
        let no_sort = full
            .replace(" │ synced", "")
            .replace("   sort: oldest waiting", "   synced");
        assert_eq!(h(w - 1).trim_end(), no_sort);
        let wn = no_sort.chars().count();
        assert_eq!(
            h(wn - 1).trim_end(),
            "Slack inbox │ 9 need attention   synced 2 min ago"
        );
        assert_eq!(h(40).trim_end(), "Slack inbox │ 9 need attention");
        assert_eq!(h(20).trim_end(), "Slack inbox │ 9 nee…");
    }

    #[test]
    fn wide_120() {
        insta::assert_snapshot!(text(&draw(&app(), 120, 36)));
    }

    #[test]
    fn medium_80() {
        insta::assert_snapshot!(text(&draw(&app(), 80, 32)));
    }

    #[test]
    fn narrow_40() {
        insta::assert_snapshot!(text(&draw(&app(), 40, 32)));
    }

    #[test]
    fn expanded_80() {
        let mut a = app();
        press(&mut a, ' ');
        insta::assert_snapshot!(text(&draw(&a, 80, 24)));
    }

    #[test]
    fn empty_80() {
        let mut a = App::new(Config::default());
        let mut c = sample_cache(now());
        c.rows.clear();
        a.set_cache(c);
        insta::assert_snapshot!(text(&draw(&a, 80, 12)));
    }

    #[test]
    fn loading_80() {
        let mut a = App::new(Config::default());
        a.set_loading(true);
        insta::assert_snapshot!(text(&draw(&a, 80, 12)));
    }

    #[test]
    fn auth_error_80() {
        let mut a = app();
        a.set_auth_error(token::MISSING_TOKEN.to_string());
        insta::assert_snapshot!(text(&draw(&a, 80, 12)));
    }

    #[test]
    fn warning_80() {
        let mut a = App::new(Config::default());
        let mut c = sample_cache(now());
        c.warnings = vec![
            "search.messages: timed out".into(),
            "conversations.history: ratelimited".into(),
        ];
        a.set_cache(c);
        insta::assert_snapshot!(text(&draw(&a, 80, 32)));
    }

    #[test]
    fn auth_banner_is_red_with_white_text() {
        let mut a = app();
        a.set_auth_error(token::MISSING_TOKEN.to_string());
        let buf = draw(&a, 80, 12);
        let cell = &buf.content[80 + 5]; // list line 1, inside the text
        assert_eq!((cell.bg, cell.fg), (Color::Red, Color::White));
        assert_eq!(buf.content[80 + 79].bg, Color::Red); // padded to the full width
    }

    #[test]
    fn non_auth_error_shows_age_in_status() {
        let mut a = app();
        let mut c = sample_cache(now());
        c.error = Some(SlackError::Http("search.messages: timed out".into()));
        a.set_cache(c);
        let lines = text_lines(&draw(&a, 100, 36));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("⚠ search.messages: timed out · data from 2 min ago")),
            "{lines:#?}"
        );
    }

    #[test]
    fn line2_keeps_types() {
        let mut a = App::new(Config::default());
        let mut c = sample_cache(now());
        c.names
            .users
            .insert("U0MARCUS".into(), "Marcus-Alexander Lee-Fitzgerald".into());
        assert_eq!(c.names.users["U0MARCUS"].chars().count(), 31);
        a.set_cache(c);
        let lines = text_lines(&draw(&a, 40, 32));
        let line = lines
            .iter()
            .find(|l| l.contains("[watch]") && l.contains("[mention]"))
            .unwrap_or_else(|| panic!("{lines:#?}"));
        assert!(line.contains("[thread]"), "{line}");
        assert!(line.contains("Marc…"), "{line}");
    }

    #[test]
    fn selected_has_marker_no_background() {
        let buf = draw(&app(), 80, 32);
        let w = buf.area.width as usize;
        let lines = text_lines(&buf);
        let y = lines.iter().position(|l| l.starts_with('▌')).unwrap();
        // Only the first row is selected.
        assert_eq!(lines.iter().filter(|l| l.starts_with('▌')).count(), 1);
        let row = &buf.content[y * w..(y + 1) * w];
        assert!(row.iter().all(|c| c.bg == Color::Reset));
        assert!(row[0].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn scroll_keeps_the_selected_row_visible() {
        let mut a = app();
        press(&mut a, 'G');
        let lines = text_lines(&draw(&a, 80, 12));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("▌") && l.contains("Reminder: deploy")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Dana Okafor")),
            "{lines:#?}"
        );
    }
}
