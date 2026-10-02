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
    /// **`djev` over LunaRoute** — the same Jev mechanism served remotely at POST /v1/systemone.
    /// No weights, no RAM floor, no GPU: a new machine needs network access only. The network
    /// dependency is covered by `on_unavailable`, which degrades to the ladder rather than silence.
    LunaRoute,
}

impl ModelKind {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "jev" | "diffgemma" => Some(Self::Jev),
            "gemma4" | "gemma" => Some(Self::Gemma4),
            "lunaroute" | "djev" => Some(Self::LunaRoute),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Gemma4 => "gemma4",
            Self::LunaRoute => "lunaroute",
        }
    }
}

/// The COMPRESSED policy for the remote route.
///
/// The LunaRoute System One backend REJECTS an `instructions` block past ~350 words with
/// `systemone_invalid_request: the System One backend rejected the request as invalid` (measured:
/// 346 words -> 200, 412 words -> 400). The full local policy does not fit, so the remote route
/// carries this shorter one. Measured on the same 80 novel probes: **0/40 dropped, 0/40 false
/// alarms, accuracy 1.000** at 0.30 s per decision.
const POLICY_LUNAROUTE: &str = r#"You are the household's front desk. Decide ONE thing: does the house have something TO DO about this message?

YES — the assistants should respond — when:
- it is a request, question or instruction to the assistants, however terse: 'rundown', 'change wed to pesto', 'can you add wine';
- it asks the house to remember something: "don't let me forget my sister's birthday";
- it states a household need, constraint or errand: "we're out of coffee", "I'm away wednesday to friday", 'I need to drop the parcel at the post office';
- it asks about the house's state or supplies: "what's for dinner?", "what's in my calendar?";
- it asks the house a QUESTION about anything — the house can answer or hand it on: 'is the pool open on sunday', 'what time does the pharmacy close';
- it reports a FAULT or planned upkeep IN THE HOUSE, even as an observation: 'the boiler is making a weird noise', "the dishwasher isn't draining properly", 'we should get the gutters cleaned'.

NO — stay out — when:
- it is personal feeling or venting: 'my back is killing me', 'work has been mad lately';
- it is a bare observation with no fault and no request: 'the garden looks a mess', 'the supermarket was packed';
- it is one family member addressing another: 'can you pick up the kids';
- it is social noise: 'night', 'thanks', 'hey';
- it REPORTS something about people or places outside the house and asks nothing: "Erik says he'll bring the wine", 'the coffee machine at work is broken', "it's meant to be warmer next week".

If a family member is ANSWERING a question the house asked, that is YES.

The single test: does the house have something TO DO? A request, a question, a fault at home, or a need — however casually worded — is YES. A feeling, a bare remark, or news about other people, is NO."#;

/// What counts as an ask — stated, because an undefined question gets a confidently wrong answer.
const POLICY: &str = "HOW TO DECIDE. You are the household's front desk. Decide whether the\n\
family should hear from the house about this message.\n\
\n\
Answer ASK when the house has something to do — act on it, log it, or answer it:\n\
  * a request, question or instruction addressed to the assistants, however terse ('rundown',\n\
    'plans for tomorrow?', 'change wed to pesto', 'can you add wine');\n\
  * a request to REMEMBER something ('my sister's birthday is the 12th, don't let me forget',\n\
    'remind me about the bins');\n\
  * a stated household need ('we're out of coffee', 'the fridge is empty', \"I'm out of eggs\");\n\
  * a question about the household's own state or supplies ('what's for dinner?', 'what's in my\n\
    calendar?', 'what's the best way to use up the sour cream');\n\
  * a constraint the house must hold ('I'm away wednesday to friday', 'keep sunday free');\n\
  * a FAULT or planned upkeep IN THE HOUSE, even stated as a plain observation, because the house\n\
    can at least flag it or arrange it ('the boiler is making a weird noise', 'the wifi keeps\n\
    dropping upstairs', 'the dishwasher isn't draining properly', 'we should get the gutters\n\
    cleaned before winter').\n\
\n\
Answer STAY_OUT for everything else:\n\
  * PERSONAL feeling or venting, even when the house could think of something to do about it\n\
    ('my back is killing me', \"I've got a headache coming on', 'I'm exhausted today', 'work has\n\
    been mad lately');\n\
  * a bare observation with no fault and no intent ('the garden looks a mess', 'the supermarket\n\
    was packed', 'it's meant to be warmer next week');\n\
  * one family member addressing another ('can you pick up the kids', 'put your dishes in the\n\
    sink');\n\
  * a remark ABOUT other people — reporting what someone said or will do — unless it asks the\n\
    house to do something ('Erik says he'll bring the wine', \"she's starting her new job monday',\n\
    'he never listens', 'my sister is so annoying sometimes');\n\
  * social noise ('night', 'thanks', 'hey', 'good night everyone', '[BLANK_AUDIO]');\n\
  * matters OUTSIDE the house with no request attached — weather, opening hours, school term\n\
    dates, the car, a machine at someone's WORK ('the coffee machine at work is broken').\n\
\n\
If a family member is answering a question the house asked, that is ASK — the answer is ours.\n\
The dividing line is simple: does the house have something TO DO? A fault in the home or a request,\n\
however casually worded, is yes. A feeling, a bare remark, or the world outside is no.";

/// Worked examples. Nine of them, taken from the harness that measured the win.
const EXAMPLES: &str = "WORKED EXAMPLES:\n\
  add bananas to the list                       -> ASK\n\
  change wed to pesto                           -> ASK\n\
  what's for dinner?                            -> ASK\n\
  what's the best way to use up the sour cream  -> ASK\n\
  my sister's birthday is the 12th, don't let me forget -> ASK\n\
  remind me about the bins                      -> ASK\n\
  the boiler is making a weird noise            -> ASK   (a fault in the house)\n\
  the wifi keeps dropping upstairs              -> ASK   (a fault in the house)\n\
  the dishwasher isn't draining properly        -> ASK   (a fault in the house)\n\
  we should get the gutters cleaned             -> ASK   (planned upkeep)\n\
  I'm travelling wednesday to friday            -> ASK   (a constraint)\n\
  my back is killing me                         -> STAY_OUT   (personal venting)\n\
  the garden looks a mess                       -> STAY_OUT   (a bare observation, no fault)\n\
  the coffee machine at work is broken          -> STAY_OUT   (outside the house)\n\
  can you pick up the kids                      -> STAY_OUT   (to another family member)\n\
  good night everyone                           -> STAY_OUT   (social noise)\n\
  Erik says he'll bring the wine                -> STAY_OUT   (about someone else)\n\
  she's starting her new job monday             -> STAY_OUT   (about someone else)";

/// `[router]` from `.wg/config.toml`. Every field has a default, so an absent block is the standard.
#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub kind: ModelKind,
    pub endpoint: String,
    /// What to do when the endpoint cannot answer. `Pattern` is the ONLY safe value and is what
    /// "unavailable" maps to; the field exists so the intent is written down rather than implied.
    pub on_unavailable: Unavailable,
    /// Bearer token for a remote endpoint. Empty for a local server (which needs none).
    pub api_key: String,
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
            api_key: std::env::var("WG_ROUTER_API_KEY").unwrap_or_default(),
            on_unavailable: Unavailable::Pattern,
            min_confidence: 0.60,
            timeout: Duration::from_secs(8),
            enabled: true,
        }
    }
}

impl RouterConfig {
    /// Read `<root>/.wg/config.toml`'s `[router]` table.
    ///
    /// **NO `[router]` BLOCK => DISABLED.** This matters for two reasons and both are load-bearing:
    ///
    /// 1. **Hermeticity.** A scratch project root (every smoke scenario and every human flow) has no
    ///    `[router]` block, so it must not reach a model server. Before this default the router
    ///    silently pointed at `127.0.0.1:8080` from a scratch root, which stalled the human-flows
    ///    scenario for 21 minutes and made the suite depend on whether a server was up
    ///    (see `DEPLOY-ROUTERS-AND-NEXT-PASS.md` §6).
    /// 2. **Honesty.** "JEV is the standard" is then a line a household writes, not an implicit
    ///    default nobody can see. The kind still defaults to JEV; enabling is explicit.
    pub fn load(root: &Path) -> Self {
        let path = root.join(".wg").join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default().disabled();
        };
        if !text.lines().any(|l| l.trim() == "[router]") {
            return Self::default().disabled();
        }
        Self::from_toml(&text)
    }

    /// The same configuration, switched off — used when no `[router]` block is present.
    fn disabled(mut self) -> Self {
        self.enabled = false;
        self
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
                "api_key" => cfg.api_key = v.to_string(),
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
            ModelKind::LunaRoute => self.classify_lunaroute(text, context),
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
        // THE POLICY AND THE EXAMPLES ARE LOAD-BEARING, not decoration. Without them the model is
        // asked an undefined question and answers with high confidence in the wrong direction —
        // measured: "my mother is visiting thursday, can we do something nice" came back p=0.009
        // (silent) at 0.99 confidence, i.e. confidently wrong. These are the exact strings the
        // harness used to reach 0/40 dropped; omitting them was a shortcut.
        let mut instructions = String::from(POLICY);
        instructions.push_str("\n\n");
        instructions.push_str(EXAMPLES);
        if let Some(c) = context {
            for block in [&c.roles, &c.memory, &c.plan, &c.history, &c.clock]
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

    /// **`djev` over LunaRoute** — the Jev interface, verified against the live API.
    ///
    /// The shape was discovered from the endpoint's own validation (it names the field it
    /// rejects): `questions` is an OBJECT keyed by id, a question needs `type`, `noul` is the
    /// yes/no arm and returns P(yes) directly, and the policy/context belongs in `instructions`.
    /// Same policy text and same ContextBlock as the local engines — only the transport differs.
    fn classify_lunaroute(
        &self,
        text: &str,
        context: Option<&super::ContextBlock>,
    ) -> Option<(bool, f32)> {
        // The COMPRESSED policy ONLY. This backend rejects an `instructions` block past ~350 words
        // (346 -> 200, 412 -> 400), so the local policy, its examples and the context block all
        // have to stay out: appending them here is a silent HTTP 400, which the `?` below turns
        // into an abstention. That is exactly how the remote route first measured as 33/40 dropped
        // — the ladder's own number, with the model never heard from.
        let instructions = String::from(POLICY_LUNAROUTE);
        let _ = context; // the remote route cannot carry the context block; the policy stands alone
        let body = serde_json::json!({
            "model": "djev",
            "state": text,
            "questions": { "house": { "type": "noul", "instructions": instructions } }
        });
        let mut req = self
            .http
            .post(format!(
                "{}/v1/systemone",
                self.cfg.endpoint.trim_end_matches('/')
            ))
            .json(&body);
        if !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        let v: serde_json::Value = req.send().ok()?.json().ok()?;
        let p = v["answers"]["house"]["noul"].as_f64()? as f32;
        let conf = if p >= 0.5 { p } else { 1.0 - p };
        if conf < self.cfg.min_confidence {
            return None;
        }
        Some((p >= 0.5, conf))
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
                for block in [&c.roles, &c.memory, &c.plan, &c.history, &c.clock]
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
        // NO CONFIG => DISABLED: a scratch root must never reach a live model server.
        let from_missing = RouterConfig::load(Path::new("/nonexistent-root"));
        assert_eq!(
            from_missing.kind,
            ModelKind::Jev,
            "the standard is still JEV"
        );
        assert!(
            !from_missing.enabled,
            "and it is OFF until a household enables it explicitly"
        );
        assert!(
            ModelClient::new(from_missing).is_none(),
            "so no client can be built"
        );
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
