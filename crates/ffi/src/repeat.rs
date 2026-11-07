//! The repeat editor for the app: the presets for a draft's date, the
//! custom rule, the sentence and next dates a rule makes, and why the
//! draft's calendar can't hold one. Rules cross as RRULE text; Swift keeps
//! no repeat rules of its own.

use chrono::{Datelike, Duration, NaiveDate, Utc, Weekday};
use jolta_core::calendar::{self as cal, CalendarId, Span, draft, recur, repeat, tz};
use jolta_core::store;

use crate::editor::{EventDraft, rule_of};
use crate::{Core, CoreError, Result};

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatPresetKind {
    Never,
    Hourly,
    Daily,
    Weekly,
    Weekdays,
    MonthDay,
    MonthNth,
    MonthLast,
    Yearly,
}

impl From<repeat::Preset> for RepeatPresetKind {
    fn from(p: repeat::Preset) -> Self {
        use repeat::Preset as P;
        match p {
            P::Never => RepeatPresetKind::Never,
            P::Hourly => RepeatPresetKind::Hourly,
            P::Daily => RepeatPresetKind::Daily,
            P::Weekly => RepeatPresetKind::Weekly,
            P::Weekdays => RepeatPresetKind::Weekdays,
            P::MonthDay => RepeatPresetKind::MonthDay,
            P::MonthNth => RepeatPresetKind::MonthNth,
            P::MonthLast => RepeatPresetKind::MonthLast,
            P::Yearly => RepeatPresetKind::Yearly,
        }
    }
}

/// One line of the repeat editor's list.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RepeatPreset {
    pub kind: RepeatPresetKind,
    /// "Every month on the fourth Tuesday".
    pub label: String,
    /// The RRULE to put in the draft's `rule`; None for Does not repeat.
    pub rule: Option<String>,
    /// Why the draft's calendar can't hold it: greyed with this line.
    pub refusal: Option<String>,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatUnit {
    Day,
    Week,
    Month,
    Year,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum RepeatEnds {
    Never,
    /// The last day an occurrence may start, "YYYY-MM-DD".
    On { day: String },
    After { count: u32 },
}

/// The Custom part: "Every [every] [unit]", the weekdays of a weekly rule
/// (1 is Monday, 7 Sunday; empty for the start's), and for a monthly rule
/// the start's weekday at its place in the month instead of its day.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct CustomRepeat {
    pub every: u32,
    pub unit: RepeatUnit,
    pub weekdays: Vec<u32>,
    pub by_weekday: bool,
    pub ends: RepeatEnds,
}

/// The repeat editor as it opens on a draft.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RepeatEditor {
    pub presets: Vec<RepeatPreset>,
    /// The preset the draft's rule is; None when only Custom says it.
    pub selected: Option<RepeatPresetKind>,
    /// The Custom part filled in from the draft's rule, or from the start's
    /// weekday when Custom can't say the rule ("Custom repeat" stays as it
    /// is unless the user picks a preset).
    pub custom: CustomRepeat,
    /// False when the rule is one Custom can't say.
    pub custom_fits: bool,
    /// The Custom part's monthly choices: "On day 24", "On the fourth Tuesday".
    pub month_day_label: String,
    pub month_weekday_label: String,
    pub summary: RuleSummary,
}

/// What a rule makes, for the editor's sentence and check.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RuleSummary {
    /// "Weekly on Tuesday and Thursday, until 22 December 2026"; "Custom
    /// repeat" when no sentence says it, None for no rule.
    pub text: Option<String>,
    /// "Thu 26 Nov", the next three after the start.
    pub next: Vec<String>,
    /// Why the draft's calendar can't hold it.
    pub refusal: Option<String>,
}

fn failed(e: impl std::fmt::Display) -> CoreError {
    CoreError::Failed { message: e.to_string() }
}

fn weekday(n: u32) -> Option<Weekday> {
    Weekday::try_from(u8::try_from(n.checked_sub(1)?).ok()?).ok()
}

fn custom_into(c: &CustomRepeat) -> Result<repeat::Custom> {
    Ok(repeat::Custom {
        every: c.every,
        unit: match c.unit {
            RepeatUnit::Day => repeat::Unit::Day,
            RepeatUnit::Week => repeat::Unit::Week,
            RepeatUnit::Month => repeat::Unit::Month,
            RepeatUnit::Year => repeat::Unit::Year,
        },
        days: c.weekdays.iter().map(|d| weekday(*d).ok_or_else(|| failed(format!("{d} is no weekday")))).collect::<Result<_>>()?,
        month_by: if c.by_weekday { repeat::MonthBy::Weekday } else { repeat::MonthBy::Day },
        ends: match &c.ends {
            RepeatEnds::Never => repeat::Ends::Never,
            RepeatEnds::After { count } => repeat::Ends::After(*count),
            RepeatEnds::On { day } => repeat::Ends::On(NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| failed(format!("{day} is not a day like 2026-12-22")))?),
        },
    })
}

fn custom_from(c: repeat::Custom) -> CustomRepeat {
    CustomRepeat {
        every: c.every,
        unit: match c.unit {
            repeat::Unit::Day => RepeatUnit::Day,
            repeat::Unit::Week => RepeatUnit::Week,
            repeat::Unit::Month => RepeatUnit::Month,
            repeat::Unit::Year => RepeatUnit::Year,
        },
        weekdays: c.days.iter().map(|d| d.number_from_monday()).collect(),
        by_weekday: c.month_by == repeat::MonthBy::Weekday,
        ends: match c.ends {
            repeat::Ends::Never => RepeatEnds::Never,
            repeat::Ends::After(count) => RepeatEnds::After { count },
            repeat::Ends::On(day) => RepeatEnds::On { day: day.format("%Y-%m-%d").to_string() },
        },
    }
}

fn day_of(span: &Span) -> NaiveDate {
    match span {
        Span::AllDay { start, .. } => *start,
        Span::Timed { start, .. } => start.wall.date(),
    }
}

impl Core {
    /// The draft's start and calendar, which presets and rules are read from.
    fn repeat_context(&self, draft: &EventDraft) -> Result<(Span, cal::Calendar)> {
        let d = draft::EventDraft::try_from(draft)?;
        let calendar = self.jolta.store.read(|c| store::calendar(c, CalendarId(draft.calendar_id)))?;
        Ok((d.span, calendar))
    }

    fn summary_of(&self, rule: Option<&cal::Recurrence>, span: &Span, calendar: &cal::Calendar) -> RuleSummary {
        let here = tz::here();
        let Some(rule) = rule else { return RuleSummary { text: None, next: Vec::new(), refusal: None } };
        let after = match span.bounds(here) {
            cal::Bounds::Utc { start, .. } => start,
            cal::Bounds::Days { start, .. } => tz::instant(NaiveDate::default().and_hms_opt(0, 0, 0).unwrap_or_default() + Duration::days(start.into()), here),
        } + Duration::seconds(1);
        let next = repeat::next(rule, span, after.max(Utc::now() - Duration::days(1)), 3, here)
            .iter()
            .map(|s| day_of(s).format("%a %-d %b").to_string())
            .collect();
        RuleSummary {
            text: Some(recur::describe(rule, span).unwrap_or_else(|| "Custom repeat".into())),
            next,
            refusal: repeat::refusal(rule, span, calendar, here),
        }
    }
}

#[uniffi::export]
impl Core {
    /// The repeat editor on `draft`: presets from its own date, greyed where
    /// its calendar can't hold them, and its rule as preset or custom.
    pub fn repeat_editor(&self, draft: EventDraft) -> Result<RepeatEditor> {
        let here = tz::here();
        let (span, calendar) = self.repeat_context(&draft)?;
        let (rule, _) = rule_of(draft.rule.as_deref(), draft.rule_was.as_deref())?;
        let presets = repeat::presets(&span, &calendar, here)
            .into_iter()
            .map(|c| RepeatPreset { kind: c.preset.into(), label: c.label, rule: c.rule, refusal: c.refusal })
            .collect();
        let selected = repeat::preset_of(rule.as_ref(), &span, &calendar, here).map(Into::into);
        let parsed = rule.as_ref().and_then(|r| match r.rules.as_slice() {
            [one] if r.rdates.is_empty() => repeat::custom_of(one, &span),
            _ => None,
        });
        let custom_fits = rule.is_none() || parsed.is_some();
        let custom = custom_from(parsed.unwrap_or(repeat::Custom {
            every: 1,
            unit: repeat::Unit::Week,
            days: vec![day_of(&span).weekday()],
            month_by: repeat::MonthBy::Day,
            ends: repeat::Ends::Never,
        }));
        let (month_day_label, month_weekday_label) = repeat::month_labels(&span);
        Ok(RepeatEditor { presets, selected, custom, custom_fits, month_day_label, month_weekday_label, summary: self.summary_of(rule.as_ref(), &span, &calendar) })
    }

    /// The RRULE the Custom part makes for the draft's start.
    pub fn custom_rule(&self, draft: EventDraft, custom: CustomRepeat) -> Result<String> {
        let (span, _) = self.repeat_context(&draft)?;
        repeat::build(&custom_into(&custom)?, &span).map_err(failed)
    }

    /// The sentence, next dates and refusal for `rule` on the draft, as the
    /// editor shows while the user changes it.
    pub fn describe_rule(&self, draft: EventDraft, rule: Option<String>) -> Result<RuleSummary> {
        let (span, calendar) = self.repeat_context(&draft)?;
        let (rule, _) = rule_of(rule.as_deref(), draft.rule_was.as_deref())?;
        Ok(self.summary_of(rule.as_ref(), &span, &calendar))
    }
}
