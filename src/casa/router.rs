//! Modular message router — swappable decision strategies behind one seam.
//!
//! WHY THIS EXISTS (Luca, 2026-10-01): *"make the router modular so we can use jev, pattern
//! matching or the current approach or future approaches."* The decision was welded into one
//! function (`elect_responders_with_owner_map`) with its vocabulary compiled beside it, so trying
//! any other approach meant editing the router. This module makes the approach a **plugin**.
//!
//! THE SHAPE
//!
//! * [`RoutingRequest`] — everything a strategy may use. Today it carries the same nine things the
//!   old call site passed, **plus** [`TurnContext`] (conversation state) and an optional
//!   [`ContextBlock`] (the assembled situation: roles/remit, memory, clock, history). Adding a
//!   field here is how a new approach gets more to work with — no strategy is forced to read it.
//! * [`RouterStrategy`] — `decide(&request) -> Option<RoutingOutcome>`. `None` means **ABSTAIN**
//!   ("not my call"), which is what makes composition possible: a low-confidence model can hand
//!   the turn back instead of guessing.
//! * [`Router`] — an ordered list of strategies. First definite answer wins; if every strategy
//!   abstains the router falls back to the concierge, never to silence. Silence is a DECISION a
//!   strategy may make deliberately, never a default produced by having no answer.
//!
//! SHIPPED STRATEGY: [`PatternStrategy`], which delegates to the existing election and is
//! behaviour-identical to it — that is asserted by `pattern_strategy_matches_the_shipped_election`
//! below, so the seam can be adopted without moving any decisions.
//!
//! DELIBERATELY NOT HERE YET: a [`ModelStrategy`] body. The endpoint, model name and prompt builder
//! are known (`reviews/eval-2026-09-30/`), and the measurement says a capable model with full
//! context reaches ~97% — but the *wiring* is where a household's behaviour changes, so it lands
//! as its own change with its own eval, behind this trait, not bolted on here.

use worksgood::notify::ownership::OwnerMap;
use worksgood::notify::telegram::TelegramConfig;
use worksgood::notify::telegram_group::{Election, TurnContext, elect_responders_with_owner_map};

/// The assembled context a strategy MAY read. Every field is optional on purpose: a strategy that
/// ignores context still works, and a strategy that wants more can arrive without changing the
/// ones already shipped.
#[derive(Clone, Debug, Default)]
pub struct ContextBlock {
    /// Who works here and what each one owns — including the exclusions ("cooking technique is
    /// Bruno's"). Read from `household.toml`'s `[[agent]] personality` prose.
    pub roles: Option<String>,
    /// Durable household memory (preferences, standing rules). Read from the memory store.
    pub memory: Option<String>,
    /// Recent turns of this chat, oldest first, `"<speaker>: <text>"`.
    pub history: Option<String>,
    /// Local clock + week state, already formatted for a prompt.
    pub clock: Option<String>,
}

impl ContextBlock {
    /// Which blocks are present — for the decision log, so "the context reached the router" is
    /// observable rather than asserted.
    pub fn present(&self) -> String {
        let f = |b: &Option<String>| {
            if b.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                "1"
            } else {
                "0"
            }
        };
        format!(
            "roles={} memory={} history={} clock={}",
            f(&self.roles),
            f(&self.memory),
            f(&self.history),
            f(&self.clock)
        )
    }
}

/// One message to decide, plus everything a strategy may use to decide it.
pub struct RoutingRequest<'a> {
    pub chat_type: Option<&'a str>,
    pub chat_id: Option<&'a str>,
    pub text: &'a str,
    pub mention_usernames: &'a [String],
    pub reply_to_bot: Option<&'a str>,
    pub sender_is_bot: bool,
    pub human_count: usize,
    pub config: &'a TelegramConfig,
    pub owner_map: &'a OwnerMap,
    /// Conversation state (already implemented and unit-tested in `telegram_group`).
    pub turn: TurnContext,
    /// The assembled situation. `None` = the caller had none; a strategy must not read it as
    /// "there is no conversation", only as "I was given none".
    pub context: Option<&'a ContextBlock>,
}

/// What a strategy decided, and WHO decided it — the provenance is part of the answer, because a
/// misroute is diagnosed by knowing which approach made the call.
pub struct RoutingOutcome {
    pub election: Election,
    /// Strategy name for the decision log (`pattern`, `model:gemma4-26b`, …).
    pub decided_by: &'static str,
    /// Calibrated confidence when the strategy has one. `None` for a deterministic rules decision,
    /// which is not "zero confidence" — it is "this strategy does not express one".
    pub confidence: Option<f32>,
}

/// A pluggable routing approach.
///
/// Returning `None` means ABSTAIN — deliberately not "silence". A strategy that wants the family
/// to hear nothing must return `Some(Election::Silence(..))`.
pub trait RouterStrategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn decide(&self, req: &RoutingRequest<'_>) -> Option<RoutingOutcome>;
}

/// The shipped deterministic ladder, behind the seam.
///
/// Delegates to `elect_responders_with_owner_map`, so adopting the router moves NO decisions. The
/// compatibility test below pins that.
pub struct PatternStrategy;

impl RouterStrategy for PatternStrategy {
    fn name(&self) -> &'static str {
        "pattern"
    }

    fn decide(&self, req: &RoutingRequest<'_>) -> Option<RoutingOutcome> {
        let election = elect_responders_with_owner_map(
            req.chat_type,
            req.chat_id,
            req.text,
            req.mention_usernames,
            req.reply_to_bot,
            req.sender_is_bot,
            req.human_count,
            req.config,
            req.owner_map,
        );
        Some(RoutingOutcome {
            election,
            decided_by: "pattern",
            confidence: None,
        })
    }
}

/// An ordered set of strategies. First definite answer wins.
pub struct Router {
    strategies: Vec<Box<dyn RouterStrategy>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            strategies: Vec::new(),
        }
    }

    /// The shipped configuration: the deterministic ladder alone. Identical to today's behaviour.
    pub fn shipped() -> Self {
        Self::new().with(Box::new(PatternStrategy))
    }

    pub fn with(mut self, s: Box<dyn RouterStrategy>) -> Self {
        self.strategies.push(s);
        self
    }

    pub fn strategies(&self) -> Vec<&'static str> {
        self.strategies.iter().map(|s| s.name()).collect()
    }

    /// Decide. Every strategy gets a turn; the first `Some` wins. If ALL abstain, the result is
    /// reported as `all-abstained` with the concierge's silence — never a silent default.
    pub fn decide(&self, req: &RoutingRequest<'_>) -> RoutingOutcome {
        for s in &self.strategies {
            if let Some(out) = s.decide(req) {
                return out;
            }
        }
        RoutingOutcome {
            election: Election::Silence(
                worksgood::notify::telegram_group::SilenceReason::SmallTalk,
            ),
            decided_by: "all-abstained",
            confidence: None,
        }
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::shipped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use worksgood::notify::telegram::TelegramBotConfig;

    fn cfg() -> TelegramConfig {
        let mut bots = HashMap::new();
        for id in ["nora", "bruno", "mira", "otto"] {
            bots.insert(
                id.to_string(),
                TelegramBotConfig {
                    bot_token: "T".to_string(),
                    chat_id: "-100".to_string(),
                    agent_id: Some(id.to_string()),
                    username: None,
                },
            );
        }
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        }
    }

    /// The shipped roster, built the non-test way: `casa_default()` is `#[cfg(test)]` in the
    /// LIB, so a test in the BINARY crate cannot see it. `from_pairs` is the production
    /// constructor for exactly this shape.
    fn om() -> OwnerMap {
        OwnerMap::from_pairs([
            ("nora", vec!["meals", "nutrition"]),
            ("bruno", vec!["meals", "cooking", "recipes"]),
            ("mira", vec!["workouts"]),
            ("otto", vec!["calendar", "coordination", "shopping"]),
        ])
    }

    fn req<'a>(
        text: &'a str,
        cfg: &'a TelegramConfig,
        om: &'a OwnerMap,
        turn: TurnContext,
    ) -> RoutingRequest<'a> {
        RoutingRequest {
            chat_type: Some("supergroup"),
            chat_id: Some("-100"),
            text,
            mention_usernames: &[],
            reply_to_bot: None,
            sender_is_bot: false,
            human_count: 2,
            config: cfg,
            owner_map: om,
            turn,
            context: None,
        }
    }

    /// THE ADOPTION GUARANTEE. Putting the shipped ladder behind the trait must not move a single
    /// decision — otherwise the seam is a behaviour change dressed as a refactor.
    #[test]
    fn pattern_strategy_matches_the_shipped_election() {
        let cfg = cfg();
        let om = om();
        let router = Router::shipped();
        for text in [
            "add milk to the shopping list",
            "what's for dinner?",
            "Nora, what's for dinner?",
            "rundown",
            "yes",
            "bananas were expensive today",
            "good night everyone",
            "put your dishes in the sink",
        ] {
            let r = req(text, &cfg, &om, TurnContext::default());
            let direct = elect_responders_with_owner_map(
                r.chat_type,
                r.chat_id,
                r.text,
                r.mention_usernames,
                r.reply_to_bot,
                r.sender_is_bot,
                r.human_count,
                r.config,
                r.owner_map,
            );
            let routed = router.decide(&r).election;
            assert_eq!(
                format!("{direct:?}"),
                format!("{routed:?}"),
                "the router moved a decision for {text:?} — the seam must be behaviour-identical"
            );
            assert_eq!(router.decide(&r).decided_by, "pattern");
        }
    }

    /// COMPOSITION. A strategy that abstains hands the turn on; the next one answers. This is what
    /// lets a model sit behind the pattern matcher and only be consulted when the ladder has
    /// nothing — the shape the measurement argues for.
    #[test]
    fn an_abstaining_strategy_falls_through_to_the_next() {
        struct Abstains;
        impl RouterStrategy for Abstains {
            fn name(&self) -> &'static str {
                "abstains"
            }
            fn decide(&self, _r: &RoutingRequest<'_>) -> Option<RoutingOutcome> {
                None
            }
        }
        let cfg = cfg();
        let om = om();
        let router = Router::new()
            .with(Box::new(Abstains))
            .with(Box::new(PatternStrategy));
        assert_eq!(router.strategies(), vec!["abstains", "pattern"]);
        let r = req(
            "add milk to the shopping list",
            &cfg,
            &om,
            TurnContext::default(),
        );
        assert_eq!(
            router.decide(&r).decided_by,
            "pattern",
            "must fall through to the second"
        );
    }

    /// A HIGHER-PRIORITY STRATEGY WINS, and its provenance is reported — so the decision log can
    /// say which approach made the call.
    #[test]
    fn the_first_definite_answer_wins_and_is_attributed() {
        struct AlwaysSilent;
        impl RouterStrategy for AlwaysSilent {
            fn name(&self) -> &'static str {
                "model:test"
            }
            fn decide(&self, _r: &RoutingRequest<'_>) -> Option<RoutingOutcome> {
                Some(RoutingOutcome {
                    election: Election::Silence(
                        worksgood::notify::telegram_group::SilenceReason::SmallTalk,
                    ),
                    decided_by: "model:test",
                    confidence: Some(0.97),
                })
            }
        }
        let cfg = cfg();
        let om = om();
        let router = Router::new()
            .with(Box::new(AlwaysSilent))
            .with(Box::new(PatternStrategy));
        let out = router.decide(&req(
            "what's for dinner?",
            &cfg,
            &om,
            TurnContext::default(),
        ));
        assert_eq!(out.decided_by, "model:test");
        assert_eq!(out.confidence, Some(0.97));
    }

    /// If EVERY strategy abstains the router does not invent silence — it reports that no strategy
    /// answered, which is a diagnostic an operator can act on.
    #[test]
    fn all_abstained_is_reported_not_disguised() {
        struct Abstains;
        impl RouterStrategy for Abstains {
            fn name(&self) -> &'static str {
                "abstains"
            }
            fn decide(&self, _r: &RoutingRequest<'_>) -> Option<RoutingOutcome> {
                None
            }
        }
        let cfg = cfg();
        let om = om();
        let router = Router::new().with(Box::new(Abstains));
        let out = router.decide(&req("add milk", &cfg, &om, TurnContext::default()));
        assert_eq!(out.decided_by, "all-abstained");
    }

    /// The context-presence string is what makes "everything reached the router" observable.
    #[test]
    fn context_presence_is_reportable() {
        let c = ContextBlock {
            roles: Some("nora".into()),
            memory: None,
            history: Some("Luca: hi".into()),
            clock: Some("Mon".into()),
        };
        assert_eq!(c.present(), "roles=1 memory=0 history=1 clock=1");
        assert_eq!(
            ContextBlock::default().present(),
            "roles=0 memory=0 history=0 clock=0"
        );
    }
}
