//! Splitting what Detect found into the two things a person is choosing between: an API provider
//! (a key, billed per token) and a harness (a coding agent already installed). Without the split,
//! eight more providers push the harnesses off the screen.

use craft_anything_ai::Detected;

/// Which half of the detected list is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DetectTab {
    #[default]
    Providers,
    Harnesses,
}

/// The tab a found backend belongs to: an agent program is a harness, everything else answers over
/// HTTP and is a provider (a local model server included).
pub fn tab_of(d: &Detected) -> DetectTab {
    if d.suggestion.kind.is_harness() { DetectTab::Harnesses } else { DetectTab::Providers }
}

/// How many of each were found, for the tab labels.
pub fn counts(detected: &[Detected]) -> (usize, usize) {
    detected.iter().fold((0, 0), |(p, h), d| match tab_of(d) {
        DetectTab::Providers => (p + 1, h),
        DetectTab::Harnesses => (p, h + 1),
    })
}

/// Which tab to show. A tab the person clicked stays, even when it is empty: that is how its "nothing
/// of this kind was found" line is read. Only the automatic first choice prefers the tab that has
/// something, so adding the only provider never opens on an empty screen.
pub fn resolve(chosen: Option<DetectTab>, providers: usize, harnesses: usize) -> DetectTab {
    match chosen {
        Some(tab) => tab,
        None if providers == 0 && harnesses > 0 => DetectTab::Harnesses,
        None => DetectTab::Providers,
    }
}

/// Draw the two tabs and return the one that is showing. An empty tab is dimmed, not disabled: it
/// still opens, and says what is missing.
pub fn bar(ui: &mut egui::Ui, chosen: &mut Option<DetectTab>, detected: &[Detected]) -> DetectTab {
    let (providers, harnesses) = counts(detected);
    let showing = resolve(*chosen, providers, harnesses);
    ui.horizontal_wrapped(|ui| {
        for (tab, label, count) in [(DetectTab::Providers, "Providers", providers), (DetectTab::Harnesses, "Harnesses", harnesses)] {
            let mut text = egui::RichText::new(format!("{} ({count})", crate::i18n::tr(label)));
            if count == 0 {
                text = text.weak();
            }
            if ui.selectable_label(showing == tab, text).clicked() {
                *chosen = Some(tab);
            }
        }
    });
    showing
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_anything_ai::{BackendConfig, BackendKind, Detected};

    fn detected(kind: BackendKind) -> Detected {
        let suggestion = BackendConfig { name: "X".into(), kind, model: "m".into(), base_url: None, ssh_host: None, enabled: true };
        Detected { suggestion, reason: String::new(), needs_key: false }
    }

    /// All backends are NVIDIA API providers; none are agent apps.
    #[test]
    fn all_backends_are_providers() {
        assert_eq!(tab_of(&detected(BackendKind::Nvidia)), DetectTab::Providers);
        let all = [detected(BackendKind::Nvidia), detected(BackendKind::Nvidia)];
        assert_eq!(counts(&all), (2, 0));
    }

    /// The first choice prefers the tab with something; a click is always honoured, so an empty tab
    /// can show what it is missing.
    #[test]
    fn the_default_gives_way_but_a_click_does_not() {
        assert_eq!(resolve(None, 0, 3), DetectTab::Harnesses, "only harnesses were found");
        assert_eq!(resolve(None, 2, 0), DetectTab::Providers);
        assert_eq!(resolve(None, 0, 0), DetectTab::Providers, "nothing at all: the first tab");
        assert_eq!(resolve(Some(DetectTab::Harnesses), 2, 0), DetectTab::Harnesses, "a clicked empty tab stays");
        assert_eq!(resolve(Some(DetectTab::Providers), 0, 3), DetectTab::Providers, "a clicked empty tab stays");
        assert_eq!(resolve(Some(DetectTab::Harnesses), 2, 3), DetectTab::Harnesses);
    }
}
