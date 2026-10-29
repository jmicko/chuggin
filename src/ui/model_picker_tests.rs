use super::*;
use ratatui::backend::TestBackend;

const FOOTER: &str =
    "Up/Down select · Enter choose · R refresh\nC connection · M model ID · Esc providers";
const LONG_DETAIL: &str = "Workstation · checked 12:34:56 · 4 models · selectable only\nModel: example-27b\nModel ID: vendor/example-27b:Q4_K_M\nNamed connection · server limits apply\nListed by this connection. Selecting a model does not generate a response; model capability and access are verified when a request runs.\nModels available through this named connection. Refresh the catalog or enter an exact model ID when the server does not offer discovery.\nCurrent selection previous:cloud is not listed by this provider.";
const SHORT_DETAIL: &str = "Workstation · checked 12:34:56 · 4 models · selectable only\nChoose an action below.\nModels available through this named connection. Refresh the catalog or enter an exact model ID when the server does not offer discovery.";

fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn outline(terminal: &Terminal<TestBackend>) -> Vec<(u16, u16, String)> {
    let buffer = terminal.backend().buffer();
    let mut positions = Vec::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer[(x, y)].symbol();
            if matches!(
                symbol,
                "┌" | "┐" | "└" | "┘" | "─" | "│" | "╭" | "╮" | "╰" | "╯"
            ) {
                positions.push((x, y, symbol.to_owned()));
            }
        }
    }
    positions
}

fn text_position(terminal: &Terminal<TestBackend>, text: &str) -> Option<(usize, usize)> {
    screen_text(terminal)
        .lines()
        .enumerate()
        .find_map(|(y, line)| line.find(text).map(|x| (x, y)))
}

#[test]
fn model_picker_bounds_stay_fixed_between_model_details_and_actions() {
    let items: Vec<String> = [
        "example-9b",
        "example-12b",
        "example-20b",
        "example-27b",
        "Refresh model list",
        "Connection settings…",
        "Enter model ID…",
        "Choose another provider",
        "Cancel selection",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for (width, height) in [(130, 42), (110, 36), (80, 24), (60, 20)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = ListState::default().with_selected(Some(3));
        terminal
            .draw(|frame| {
                render_model_selection(
                    frame,
                    "Choose model for this project",
                    &items,
                    &mut state,
                    LONG_DETAIL,
                    FOOTER,
                    0,
                );
            })
            .unwrap();
        let model_outline = outline(&terminal);
        assert!(!model_outline.is_empty());
        let header = text_position(&terminal, "CHUGGIN").unwrap();
        let footer = text_position(&terminal, "R refresh");
        assert!(screen_text(&terminal).contains("example-27b"));

        state.select(Some(4));
        terminal
            .draw(|frame| {
                render_model_selection(
                    frame,
                    "Choose model for this project",
                    &items,
                    &mut state,
                    SHORT_DETAIL,
                    FOOTER,
                    0,
                );
            })
            .unwrap();
        assert_eq!(
            outline(&terminal),
            model_outline,
            "Picker bounds changed at {width}×{height} when moving from a model to an action"
        );
        assert_eq!(text_position(&terminal, "CHUGGIN"), Some(header));
        assert_eq!(text_position(&terminal, "R refresh"), footer);
        assert!(screen_text(&terminal).contains("Refresh model list"));
    }
}

#[test]
fn model_picker_preserves_visible_catalog_offset_when_navigating_back_up() {
    let items: Vec<String> = (0..100).map(|index| format!("model-{index:03}")).collect();
    for (width, height) in [(110, 36), (80, 24), (42, 16)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = ListState::default().with_selected(Some(63));
        terminal
            .draw(|frame| {
                render_model_selection(
                    frame,
                    "Choose model",
                    &items,
                    &mut state,
                    LONG_DETAIL,
                    FOOTER,
                    0,
                );
            })
            .unwrap();
        assert!(state.offset() > 0);
        assert!(screen_text(&terminal).contains("model-063"));
        state.select(Some(64));
        terminal
            .draw(|frame| {
                render_model_selection(
                    frame,
                    "Choose model",
                    &items,
                    &mut state,
                    LONG_DETAIL,
                    FOOTER,
                    0,
                );
            })
            .unwrap();
        let offset = state.offset();
        assert!(screen_text(&terminal).contains("model-064"));
        assert!(screen_text(&terminal).contains("model-063"));
        state.select(Some(63));
        terminal
            .draw(|frame| {
                render_model_selection(
                    frame,
                    "Choose model",
                    &items,
                    &mut state,
                    SHORT_DETAIL,
                    FOOTER,
                    0,
                );
            })
            .unwrap();
        assert_eq!(
            state.offset(),
            offset,
            "Navigating to an already visible row moved the viewport at {width}×{height}"
        );
        assert!(screen_text(&terminal).contains("model-063"));
    }
}

#[test]
fn model_picker_tolerates_narrow_and_short_terminals_and_detail_scrolling() {
    let items = vec!["model-one".to_owned(), "Refresh model list".to_owned()];
    for (width, height) in [(80, 12), (42, 12), (24, 8), (10, 4), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = ListState::default().with_selected(Some(1));
        for offset in [0, 1, u16::MAX] {
            terminal
                .draw(|frame| {
                    render_model_selection(
                        frame,
                        "Choose model for this project with a long provider name",
                        &items,
                        &mut state,
                        LONG_DETAIL,
                        FOOTER,
                        offset,
                    );
                })
                .unwrap();
        }
        assert_eq!(
            terminal.backend().buffer().area,
            Rect::new(0, 0, width, height)
        );
    }
}

#[test]
fn model_picker_returns_clamped_detail_offset_for_immediate_scroll_back() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let items = vec!["model-one".to_owned(), "Refresh model list".to_owned()];
    let mut state = ListState::default().with_selected(Some(0));
    let mut offset = u16::MAX;
    terminal
        .draw(|frame| {
            offset = render_model_selection(
                frame,
                "Choose model",
                &items,
                &mut state,
                LONG_DETAIL,
                FOOTER,
                offset,
            );
        })
        .unwrap();
    assert!(offset > 0 && offset < u16::MAX);
    let bottom = screen_text(&terminal);
    let previous = offset;
    terminal
        .draw(|frame| {
            offset = render_model_selection(
                frame,
                "Choose model",
                &items,
                &mut state,
                LONG_DETAIL,
                FOOTER,
                offset.saturating_sub(1),
            );
        })
        .unwrap();
    assert_eq!(offset, previous - 1);
    assert_ne!(
        screen_text(&terminal),
        bottom,
        "The first upward scroll after reaching the bottom should reveal earlier text"
    );
}
