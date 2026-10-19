//! Headless tests of the launcher UI, using Slint's testing backend with mocked time.
//!
//! These only test the Slint UI (as compiled by `build.rs`), not the rest of polymodo.

slint::include_modules!();
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery, mock_elapsed_time};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};
use std::time::Duration;

/// A frame, as far as mocked time is concerned
const FRAME: Duration = Duration::from_millis(16);

/// Advance time by a frame, and do what rendering a frame would do to the element tree.
///
/// The testing backend doesn't render, but rendering is what lays out the items of a list view
/// (including the list's `viewport-height`). Visiting every element does the same.
fn frame(window: &LauncherWindow) {
    mock_elapsed_time(FRAME);
    ElementQuery::from_root(window).match_descendants().find_all();
}

/// A launcher window with enough entries to scroll through.
fn launcher_with_entries() -> LauncherWindow {
    i_slint_backend_testing::init_no_event_loop();

    let window = LauncherWindow::new().unwrap();
    let entries: Vec<LauncherEntry> = (0..100)
        .map(|i| LauncherEntry {
            name: format!("Entry {i}").into(),
            id: i,
            ..Default::default()
        })
        .collect();
    window
        .global::<LauncherEntries>()
        .set_entries(ModelRc::new(VecModel::from(entries)));
    window.show().unwrap();
    frame(&window);

    window
}

fn entry_list(window: &LauncherWindow) -> ElementHandle {
    ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::List)
        .find_first()
        .expect("the launcher has an entry list")
}

/// The centre of the entry list, in window coordinates.
fn list_centre(window: &LauncherWindow) -> LogicalPosition {
    let list = entry_list(window);
    let (position, size) = (list.absolute_position(), list.size());
    LogicalPosition::new(
        position.x + size.width / 2.0,
        position.y + size.height / 2.0,
    )
}

fn scroll_position(window: &LauncherWindow) -> f32 {
    window.get_scroll_position()
}

fn dispatch(window: &LauncherWindow, event: WindowEvent) {
    window.window().dispatch_event(event);
}

fn press(window: &LauncherWindow, position: LogicalPosition) {
    dispatch(window, WindowEvent::PointerMoved { position });
    dispatch(
        window,
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
    );
}

fn release(window: &LauncherWindow, position: LogicalPosition) {
    dispatch(
        window,
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
    );
}

/// Move the pointer in `steps` steps of `dy` each, one frame apart.
fn move_by(window: &LauncherWindow, mut position: LogicalPosition, dy: f32, steps: u32) -> LogicalPosition {
    for _ in 0..steps {
        frame(window);
        position.y += dy;
        dispatch(window, WindowEvent::PointerMoved { position });
    }
    position
}

/// Let `duration` of (mocked) time pass, frame by frame.
fn wait(window: &LauncherWindow, duration: Duration) {
    for _ in 0..duration.as_millis() / FRAME.as_millis() {
        frame(window);
    }
}

/// Scroll the list as far down as it goes, using the mouse wheel.
fn max_scroll_position(window: &LauncherWindow) -> f32 {
    let centre = list_centre(window);
    dispatch(
        window,
        WindowEvent::PointerScrolled {
            position: centre,
            delta_x: 0.0,
            delta_y: -1_000_000.0,
        },
    );
    scroll_position(window)
}

#[test]
fn mouse_wheel_scrolls() {
    let window = launcher_with_entries();
    assert_eq!(scroll_position(&window), 0.0);

    let centre = list_centre(&window);
    dispatch(&window, WindowEvent::PointerMoved { position: centre });
    dispatch(
        &window,
        WindowEvent::PointerScrolled {
            position: centre,
            delta_x: 0.0,
            delta_y: -120.0,
        },
    );

    assert_eq!(scroll_position(&window), 120.0);
}

#[test]
fn dragging_scrolls_and_flicks_with_momentum() {
    let window = launcher_with_entries();

    // drag the content up by 100px, quickly
    let start = list_centre(&window);
    press(&window, start);
    let end = move_by(&window, start, -20.0, 5);
    assert_eq!(scroll_position(&window), 100.0, "the content follows the pointer");
    release(&window, end);

    // after releasing, the list keeps going...
    let released = scroll_position(&window);
    wait(&window, Duration::from_millis(160));
    let after_first = scroll_position(&window);
    wait(&window, Duration::from_millis(160));
    let after_second = scroll_position(&window);
    assert!(after_first > released, "the list keeps moving after release");

    // ...slowing down gradually...
    assert!(
        after_second - after_first < after_first - released,
        "the flick slows down: moved {} then {}",
        after_first - released,
        after_second - after_first,
    );
    assert!(after_second > after_first, "the flick doesn't stop abruptly");

    // ...until it stops.
    wait(&window, Duration::from_secs(5));
    let stopped = scroll_position(&window);
    wait(&window, Duration::from_secs(1));
    assert_eq!(scroll_position(&window), stopped, "the flick comes to a stop");
}

#[test]
fn releasing_after_holding_still_does_not_flick() {
    let window = launcher_with_entries();

    let start = list_centre(&window);
    press(&window, start);
    let end = move_by(&window, start, -20.0, 5);
    wait(&window, Duration::from_millis(300));
    release(&window, end);

    let released = scroll_position(&window);
    wait(&window, Duration::from_millis(500));
    assert_eq!(scroll_position(&window), released);
}

#[test]
fn clicking_does_not_scroll() {
    let window = launcher_with_entries();

    let centre = list_centre(&window);
    press(&window, centre);
    frame(&window);
    release(&window, centre);
    wait(&window, Duration::from_millis(500));

    assert_eq!(scroll_position(&window), 0.0);
}

#[test]
fn dragging_the_scrollbar_scrolls() {
    let window = launcher_with_entries();
    let max = max_scroll_position(&window);
    assert!(max > 0.0, "the list is scrollable");

    // back to the top
    dispatch(
        &window,
        WindowEvent::PointerScrolled {
            position: list_centre(&window),
            delta_x: 0.0,
            delta_y: 1_000_000.0,
        },
    );
    assert_eq!(scroll_position(&window), 0.0);

    // grab the thumb, at the top right of the list
    let list = entry_list(&window);
    let grab = LogicalPosition::new(
        list.absolute_position().x + list.size().width - 4.0,
        list.absolute_position().y + 4.0,
    );
    press(&window, grab);

    // dragging it down a bit scrolls the list down, by more than the pointer moved
    let moved = move_by(&window, grab, 10.0, 5);
    let scrolled = scroll_position(&window);
    assert!(scrolled > 50.0, "scrolled {scrolled}px");
    assert!(scrolled < max);

    // dragging it past the bottom scrolls to the end
    let moved = move_by(&window, moved, 1000.0, 1);
    assert_eq!(scroll_position(&window), max);
    release(&window, moved);

    // and the scrollbar doesn't flick
    wait(&window, Duration::from_millis(500));
    assert_eq!(scroll_position(&window), max);
}
