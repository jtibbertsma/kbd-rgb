//! Undo and redo.

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;

use super::state::Scene;

/// The scenes that undo and redo move between. It holds at most `depth` scenes in each direction,
/// and the oldest scene is dropped when another one is added.
#[derive(Debug)]
pub(super) struct History {
    past: VecDeque<Scene>,
    future: VecDeque<Scene>,
    depth: usize,
}

impl History {
    pub(super) fn new(depth: NonZeroUsize) -> Self {
        History {
            past: VecDeque::new(),
            future: VecDeque::new(),
            depth: depth.get(),
        }
    }

    /// Saves `scene` for undo. Call it before a change. A change ends redo.
    pub(super) fn remember(&mut self, scene: &Scene) {
        self.keep_past(scene);
        self.future.clear();
    }

    /// Goes back to the most recent scene that's different from `scene`. When there is none, it
    /// goes back to `oldest`.
    pub(super) fn undo(&mut self, scene: &Scene, oldest: Scene) -> Scene {
        while self.past.back() == Some(scene) {
            self.past.pop_back();
        }
        let back = self.past.pop_back().unwrap_or(oldest);
        if back != *scene {
            push_bounded(&mut self.future, scene.clone(), self.depth);
        }
        back
    }

    pub(super) fn redo(&mut self, scene: &Scene) -> Option<Scene> {
        let forward = self.future.pop_back()?;
        self.keep_past(scene);
        Some(forward)
    }

    fn keep_past(&mut self, scene: &Scene) {
        if self.past.back() != Some(scene) {
            push_bounded(&mut self.past, scene.clone(), self.depth);
        }
    }
}

fn push_bounded(scenes: &mut VecDeque<Scene>, scene: Scene, depth: usize) {
    if scenes.len() == depth {
        scenes.pop_front();
    }
    scenes.push_back(scene);
}

impl fmt::Display for History {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} to undo, {} to redo",
            self.past.len(),
            self.future.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::setting::Setting;

    fn solid(color: &str) -> Scene {
        Scene::filled(&Setting::Color(color.parse().unwrap()))
    }

    fn history(depth: usize) -> History {
        History::new(NonZeroUsize::new(depth).unwrap())
    }

    fn white() -> Scene {
        Scene::filled(&Setting::Color(Color::WHITE))
    }

    #[test]
    fn undo_goes_back_one_scene_at_a_time() {
        let (red, green, blue) = (solid("FF0000"), solid("00FF00"), solid("0000FF"));
        let mut history = history(100);
        history.remember(&red);
        history.remember(&green);
        assert_eq!(history.undo(&blue, white()), green);
        assert_eq!(history.undo(&green, white()), red);
    }

    #[test]
    fn redo_comes_forward_again() {
        let (red, green, blue) = (solid("FF0000"), solid("00FF00"), solid("0000FF"));
        let mut history = history(100);
        history.remember(&red);
        history.remember(&green);
        history.undo(&blue, white());
        history.undo(&green, white());
        assert_eq!(history.redo(&red), Some(green.clone()));
        assert_eq!(history.redo(&green), Some(blue.clone()));
        assert_eq!(history.redo(&blue), None);
    }

    #[test]
    fn undo_skips_scenes_equal_to_the_current_one() {
        let (red, green) = (solid("FF0000"), solid("00FF00"));
        let mut history = history(100);
        history.remember(&red);
        history.remember(&green);
        assert_eq!(history.undo(&green, white()), red);
    }

    #[test]
    fn remembering_the_same_scene_twice_keeps_it_once() {
        let red = solid("FF0000");
        let mut history = history(100);
        history.remember(&red);
        history.remember(&red);
        assert_eq!(history.to_string(), "1 to undo, 0 to redo");
    }

    #[test]
    fn with_nothing_to_go_back_to_undo_gives_the_oldest_scene() {
        assert_eq!(history(100).undo(&solid("FF0000"), white()), white());
    }

    #[test]
    fn a_new_change_ends_redo() {
        let (red, green, blue) = (solid("FF0000"), solid("00FF00"), solid("0000FF"));
        let mut history = history(100);
        history.remember(&red);
        history.undo(&green, white());
        assert_eq!(history.to_string(), "0 to undo, 1 to redo");
        history.remember(&blue);
        assert_eq!(history.to_string(), "1 to undo, 0 to redo");
        assert_eq!(history.redo(&blue), None);
    }

    #[test]
    fn the_oldest_scene_is_dropped_past_the_depth() {
        let scenes: Vec<Scene> = (0..5).map(|i| solid(&format!("00000{i}"))).collect();
        let mut history = history(3);
        for scene in &scenes {
            history.remember(scene);
        }
        let mut now = solid("FFFFFF");
        for expected in [&scenes[4], &scenes[3], &scenes[2]] {
            now = history.undo(&now, white());
            assert_eq!(&now, expected);
        }
        assert_eq!(history.undo(&now, solid("ABCDEF")), solid("ABCDEF"));
    }
}
