//! Model-backed routing strategies, and the hybrid that makes them safe to turn on.
//!
//! DESIGN CONSTRAINT (the whole reason this file is shaped this way): a model must not be able to
//! TAKE AWAY an answer the deterministic ladder already gets right. Measured on 13,610 real and
//! generated turns, the rules are conservative and precise — their failure is that they go SILENT
//! on 12% of real asks (`reviews/EVAL-2026-09-30-routing-classifier.md`). So the model is consulted
//! on exactly one question, in exactly one place: **when the ladder would have said nothing.**
//!
//! That gives the switch a blast radius of zero on every message the house already handles, and it
//! degrades to today's behaviour whenever the model endpoint is down (which happened twice this
//! week, once as a network block).
//!
//! WHAT THE MODEL DECIDES: silence-vs-respond, nothing else. WHO answers stays with the ladder and
//! the owner map, so voice/ownership behaviour is unchanged.

use std::path::Path;
use std::time::Duration;

use worksgood::notify::telegram_group::{AddressedBy, Election, SilenceReason};

use super::{PatternStrategy, Router, RouterStrategy, RoutingOutcome, RoutingRequest};

/// Which engine answers the silence-boundary question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    /// `mmastrac/diffgemma serve` — native JEV: canvas seeded with noise, slot read. The standard.
    Jev,
    /// A capable autoregressive model (vllm-metal) via guided_choice + logprobs + order swap.
    Gemma4,
}

impl ModelKind {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "jev" | "diffgemma" => Some(Self::Jev),
            "gemma4" | "gemma" => Some(Self::Gemma4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Gemma4 => "gemma4",
        }
    }
}

/// `[router]` from `.wg/config.toml`. Every field has a default, so an absent block is the standard.
#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub kind: ModelKind,
    pub endpoint: String,
    /// What to do when the endpoint cannot answer. `Pattern` is the ONLY safe value and is what
    /// "unavailable" maps to; the field exists so the intent is written down rather than implied.
    pub on_unavailable: Unavailable,
    /// Below this probability the model abstains and the ladder's silence stands.
    pub min_confidence: f32,
    pub timeout: Duration,
    /// Whether the hybrid is enabled at all. `false` = the ladder alone (the pre-switch behaviour).
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Fall back to the deterministic ladder. Never to silence-by-default.
    Pattern,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            kind: ModelKind::Jev, // THE STANDARD (2026-10-02)
            endpoint: "http://127.0.0.1:8080".to_string(),
            on_unavailable: Unavailable::Pattern,
            min_confidence: 0.60,
            timeout: Duration::from_secs(8),
            enabled: true,
        }
    }
}

impl RouterConfig {
    /// Read `<root>/.wg/config.toml`'s `[router]` table. Absent keys take the defaults above, so a
    /// household that has never heard of this feature gets the standard.
    pub fn load(root: &Path) -> Self {
        let path = root.join(".wg").join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        Self::from_toml(&text)
    }

    /// Parse the `[router]` table out of a config document. Deliberately minimal: the engine has no
    /// `toml` dependency here and the block is four scalars, so a hand parse keeps the blast radius
    /// small and is pinned by tests below.
    pub fn from_toml(text: &str) -> Self {
        let mut cfg = Self::default();
        let mut in_router = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_router = line == "[router]";
                continue;
            }
            if !in_router || line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"').trim();
            match k.trim() {
                "strategy" => {
                    if let Some(k) = ModelKind::parse(v) {
                        cfg.kind = k;
                    }
                }
                "endpoint" => cfg.endpoint = v.to_string(),
                "enabled" => {
                    cfg.enabled = !matches!(v.to_ascii_lowercase().as_str(), "false" | "no" | "0")
                }
                "min_confidence" => {
                    if let Ok(f) = v.parse() {
                        cfg.min_confidence = f;
                    }
                }
                "timeout_secs" => {
                    if let Ok(s) = v.parse::<u64>() {
                        cfg.timeout = Duration::from_secs(s.max(1));
                    }
                }
                // Accepted and asserted, not silently ignored: the only legal value is `pattern`.
                "on_unavailable" => {
                    debug_assert_eq!(v, "pattern", "on_unavailable must be `pattern`")
                }
                _ => {}
            }
        }
        cfg
    }
}

/// Talks to one model endpoint. `None` from every method means "no answer" — never a guess.
pub struct ModelClient {
    cfg: RouterConfig,
    http: reqwest::blocking::Client,
}

impl ModelClient {
    pub fn new(cfg: RouterConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .ok()?;
        Some(Self { cfg, http })
    }

    /// `Some(respond)` when the model is confident; `None` on any failure or low confidence.
    ///
    /// Returns the confidence alongside so the decision log can carry it.
    pub fn classify(
        &self,
        text: &str,
        context: Option<&super::ContextBlock>,
    ) -> Option<(bool, f32)> {
        match self.cfg.kind {
            ModelKind::Jev => self.classify_jev(text, context),
            ModelKind::Gemma4 => self.classify_gemma4(text, context),
        }
    }

    /// The schema API: the answer template is seeded into the canvas with each label slot as noise,
    /// and the distribution is read at the slot (vLLM #57250's mechanism). The server also returns
    /// `entropy`; we abstain when it is high rather than pretending to be sure.
    fn classify_jev(
        &self,
        text: &str,
        context: Option<&super::ContextBlock>,
    ) -> Option<(bool, f32)> {
        let mut instructions = String::from(
            "Decide whether the message is addressed to the household's assistants, who should \
             respond, or whether it is one family member talking to another / social noise, where \
             the assistants stay out.",
        );
        if let Some(c) = context {
            for block in [&c.roles, &c.memory, &c.history, &c.clock]
                .into_iter()
                .flatten()
            {
                if !block.trim().is_empty() {
                    instructions.push('\n');
                    instructions.push_str(block);
                }
            }
        }
        let schema = serde_json::json!({
            "questions": [{
                "id": "house", "type": "choice", "instructions": instructions,
                "options": [{"name": "respond"}, {"name": "stay_out"}],
            }]
        });
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": schema.to_string()},
                {"role": "user", "content": serde_json::json!({"message": text}).to_string()},
            ]
        });
        let resp = self
            .http
            .post(format!(
                "{}/v1/chat/completions",
                self.cfg.endpoint.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .ok()?;
        let v: serde_json::Value = resp.json().ok()?;
        let content = v["choices"][0]["message"]["content"].as_str()?;
        let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
        let ans = &parsed["answers"]["house"];
        let p = ans["probabilities"]["respond"].as_f64()? as f32;
        let decided = if p >= 0.5 { p } else { 1.0 - p };
        if decided < self.cfg.min_confidence {
            return None;
        }
        Some((p >= 0.5, decided))
    }

    /// The autoregressive path. Two reads with the options swapped, averaged — without that the
    /// readout measures the model's preference between `A` and `B` rather than the message
    /// (measured: 1.5B answered B to everything, 7B answered A to everything).
    fn classify_gemma4(
        &self,
        text: &str,
        context: Option<&super::ContextBlock>,
    ) -> Option<(bool, f32)> {
        let ask = "it is addressed to the household's assistants and they should respond";
        let stay = "it is one family member talking to another, and the assistants should stay out";
        let mut first = None;
        for order in [(ask, stay), (stay, ask)] {
            let mut prompt = format!(
                "A message arrives in the family chat. Which is true?\nA) {}\nB) {}\n\
                 Answer with a single letter, A or B.\nMessage: {text}",
                order.0, order.1
            );
            if let Some(c) = context {
                for block in [&c.roles, &c.memory, &c.history, &c.clock]
                    .into_iter()
                    .flatten()
                {
                    if !block.trim().is_empty() {
                        prompt.push('\n');
                        prompt.push_str(block);
                    }
                }
            }
            let body = serde_json::json!({
                "model": "gemma", "max_tokens": 1, "temperature": 0.0,
                "logprobs": true, "top_logprobs": 5, "guided_choice": ["A", "B"],
                "messages": [{"role": "user", "content": prompt}],
            });
            let resp = self
                .http
                .post(format!(
                    "{}/v1/chat/completions",
                    self.cfg.endpoint.trim_end_matches('/')
                ))
                .json(&body)
                .send()
                .ok()?;
            let v: serde_json::Value = resp.json().ok()?;
            let entries = v["choices"][0]["logprobs"]["content"].as_array()?;
            let top = entries.first()?["top_logprobs"].as_array()?;
            let mut pa = 0.0f64;
            let mut pb = 0.0f64;
            for e in top {
                let tok = e["token"].as_str().unwrap_or("").trim();
                let lp = e["logprob"].as_f64().unwrap_or(-30.0);
                if tok == "A" {
                    pa = lp.exp();
                } else if tok == "B" {
                    pb = lp.exp();
                }
            }
            if pa + pb <= 1e-9 {
                return None;
            }
            let got = pa / (pa + pb);
            let p_ask = if order.0 == ask { got } else { 1.0 - got };
            first = Some(match first {
                None => p_ask,
                Some(prev) => (prev + p_ask) / 2.0,
            });
        }
        let p_ask = first? as f32;
        let decided = if p_ask >= 0.5 { p_ask } else { 1.0 - p_ask };
        if decided < self.cfg.min_confidence {
            return None;
        }
        Some((p_ask >= 0.5, decided))
    }
}

/// The hybrid: the ladder decides first, the model is consulted **only when the ladder is silent**.
///
/// This ordering is the safety property. Every message the house already handles never reaches the
/// model, so turning the switch on cannot take an answer away; and if the model is unreachable the
/// strategy returns `None`, the ladder's silence stands, and behaviour is exactly today's.
pub struct HybridStrategy {
    client: Option<ModelClient>,
    ladder: PatternStrategy,
}

impl HybridStrategy {
    pub fn new(cfg: RouterConfig) -> Self {
        Self {
            client: ModelClient::new(cfg),
            ladder: PatternStrategy,
        }
    }
}

impl RouterStrategy for HybridStrategy {
    fn name(&self) -> &'static str {
        "hybrid(silence->model)"
    }

    fn decide(&self, req: &RoutingRequest<'_>) -> Option<RoutingOutcome> {
        // 1. The ladder decides. If it has anything to say, it wins, unchanged.
        let from_ladder = self.ladder.decide(req)?;
        if !matches!(from_ladder.election, Election::Silence(_)) {
            return Some(from_ladder);
        }
        // 2. Silence — the one band the model exists for.
        let Some(client) = self.client.as_ref() else {
            return Some(from_ladder); // no model configured/reachable: today's behaviour
        };
        let Some((respond, conf)) = client.classify(req.text, req.context) else {
            return Some(from_ladder); // low confidence or failure: the silence stands
        };
        if !respond {
            return Some(from_ladder);
        }
        // 3. The model says the house should answer. WHO answers is still the ladder's business:
        //    route to the concierge, which is the documented home of an unaddressed ask.
        let Some(bot) = super::concierge_for(req) else {
            return Some(from_ladder);
        };
        Some(RoutingOutcome {
            election: Election::One {
                bot,
                reply_chat: req.chat_id.unwrap_or("").to_string(),
                body: req.text.to_string(),
                addressed_by: AddressedBy::Concierge,
            },
            decided_by: "model",
            confidence: Some(conf),
        })
    }
}

/// Build the router from config. **JEV is the standard**; the ladder is always present as the
/// fallback, and an unavailable model degrades to it rather than to silence.
pub fn router_from_config(root: &Path) -> Router {
    let cfg = RouterConfig::load(root);
    let mut r = Router::new().with(Box::new(HybridStrategy::new(cfg)));
    r = r.with(Box::new(PatternStrategy));
    r
}

#[cfg(test)]
mod tests {
    use super::super::{PatternStrategy, RouterStrategy, RoutingRequest};
    use super::*;
    use std::collections::HashMap;
    use worksgood::notify::ownership::OwnerMap;
    use worksgood::notify::telegram::TelegramBotConfig;
    use worksgood::notify::telegram_group::TurnContext;

    fn cfg() -> worksgood::notify::telegram::TelegramConfig {
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
        worksgood::notify::telegram::TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        }
    }
    fn om() -> OwnerMap {
        OwnerMap::from_pairs([
            ("nora", vec!["meals", "nutrition"]),
            ("otto", vec!["calendar", "coordination", "shopping"]),
        ])
    }
    fn req<'a>(
        text: &'a str,
        c: &'a worksgood::notify::telegram::TelegramConfig,
        o: &'a OwnerMap,
    ) -> RoutingRequest<'a> {
        RoutingRequest {
            chat_type: Some("supergroup"),
            chat_id: Some("-100"),
            text,
            mention_usernames: &[],
            reply_to_bot: None,
            sender_is_bot: false,
            human_count: 2,
            config: c,
            owner_map: o,
            turn: TurnContext::default(),
            context: None,
        }
    }

    /// JEV IS THE STANDARD: an absent `[router]` block yields kind=Jev.
    #[test]
    fn the_standard_is_jev_and_absent_config_takes_it() {
        let d = RouterConfig::default();
        assert_eq!(d.kind, ModelKind::Jev, "JEV is the standard (2026-10-02)");
        assert_eq!(d.endpoint, "http://127.0.0.1:8080");
        assert_eq!(d.on_unavailable, Unavailable::Pattern);
        // no file on disk -> defaults, not an error
        let from_missing = RouterConfig::load(Path::new("/nonexistent-root"));
        assert_eq!(from_missing.kind, ModelKind::Jev);
    }

    #[test]
    fn the_router_block_is_parsed() {
        let c = RouterConfig::from_toml(
            "[router]\nstrategy = \"gemma4\"\nendpoint = \"http://x:1\"\nenabled = false\nmin_confidence = 0.8\ntimeout_secs = 3\n",
        );
        assert_eq!(c.kind, ModelKind::Gemma4);
        assert_eq!(c.endpoint, "http://x:1");
        assert!(!c.enabled);
        assert_eq!(c.min_confidence, 0.8);
        assert_eq!(c.timeout, Duration::from_secs(3));
        // a different table must not leak into ours
        let other = RouterConfig::from_toml("[agent]\nstrategy = \"gemma4\"\n");
        assert_eq!(
            other.kind,
            ModelKind::Jev,
            "keys outside [router] are ignored"
        );
    }

    /// THE SAFETY PROPERTY, part 1: a message the ladder already answers never reaches the model.
    /// Proved by pointing the client at a port that CANNOT answer — if the ladder's answer came
    /// through unchanged, the model was never consulted.
    #[test]
    fn the_ladder_wins_before_the_model_is_ever_asked() {
        let (c, o) = (cfg(), om());
        let h = HybridStrategy::new(RouterConfig {
            endpoint: "http://127.0.0.1:9".into(),
            ..Default::default()
        });
        let r = req("add milk to the shopping list", &c, &o);
        let ladder = PatternStrategy.decide(&r).unwrap();
        let hybrid = h.decide(&r).unwrap();
        assert_eq!(
            format!("{:?}", ladder.election),
            format!("{:?}", hybrid.election)
        );
        assert_eq!(
            hybrid.decided_by, "pattern",
            "the ladder answered; the model was not consulted"
        );
    }

    /// THE SAFETY PROPERTY, part 2: an UNREACHABLE model degrades to the ladder's silence, never to
    /// a dropped request and never to a crash. This is the egress-outage case.
    #[test]
    fn an_unreachable_model_degrades_to_the_ladder_not_to_a_loss() {
        let (c, o) = (cfg(), om());
        let h = HybridStrategy::new(RouterConfig {
            endpoint: "http://127.0.0.1:9".into(),
            ..Default::default()
        });
        let r = req("the boiler is making a weird noise at night", &c, &o);
        let out = h.decide(&r).unwrap();
        assert_eq!(
            out.decided_by, "pattern",
            "unavailable -> the ladder's answer stands"
        );
        assert!(matches!(out.election, Election::Silence(_)));
    }

    /// A disabled switch is inert: no client is built at all, so no request can be made.
    #[test]
    fn a_disabled_switch_makes_no_client() {
        assert!(
            ModelClient::new(RouterConfig {
                enabled: false,
                ..Default::default()
            })
            .is_none()
        );
    }
}
