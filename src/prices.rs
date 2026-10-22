//! Model prices, from the list ccusage itself prices with.
//!
//! ccmoneta carries no price table of its own: a hard-coded one is what made
//! CCMeter bill Opus 5 at Sonnet rates without a word (see docs/decisions.md).
//! The rates come from LiteLLM's public model list, the same source as
//! ccusage's, downloaded with curl at most once a day and kept in the cache. A
//! failed download keeps the copy already there. A model the list does not
//! name is never guessed at: it costs nothing and is reported as unpriced, and
//! every surface that shows spend says so.
//!
//! Matching and the formula follow ccusage's, so the figures agree with it:
//! an exact key first, then the longest key that contains the model name (or
//! is contained by it) at word boundaries; tokens above 200k in one request
//! at a bucket's `_above_200k_tokens` rate where the list has one; a one-hour
//! cache write at twice the input rate; fast mode at the entry's multiplier.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::store;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// How long a downloaded list is used before it is fetched again.
const MAX_AGE: i64 = 24 * 3600;

/// Above this many tokens of one kind in one request, the `_above_200k` rates
/// apply to the excess.
const TIER: u64 = 200_000;

/// One model's rates, in dollars per token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub input_above: Option<f64>,
    pub output_above: Option<f64>,
    pub cache_write_above: Option<f64>,
    pub cache_read_above: Option<f64>,
    /// The multiplier for a request made in fast mode.
    pub fast: f64,
}

/// The tokens of one request, as its transcript line records them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    /// Cache writes kept for five minutes, the default.
    pub cache_5m: u64,
    /// Cache writes kept for an hour, billed at twice the input rate.
    pub cache_1h: u64,
    pub cache_read: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_5m + self.cache_1h + self.cache_read
    }
}

/// `n` tokens at `base`, the part above 200k at `above` where there is one.
fn tiered(n: u64, base: f64, above: Option<f64>) -> f64 {
    match above {
        Some(above) if n > TIER => TIER as f64 * base + (n - TIER) as f64 * above,
        _ => n as f64 * base,
    }
}

impl Rate {
    pub fn cost(&self, t: &Tokens, fast: bool) -> f64 {
        let cost = tiered(t.input, self.input, self.input_above)
            + tiered(t.output, self.output, self.output_above)
            + tiered(t.cache_5m, self.cache_write, self.cache_write_above)
            + tiered(
                t.cache_1h,
                self.input * 2.0,
                self.input_above.map(|r| r * 2.0),
            )
            + tiered(t.cache_read, self.cache_read, self.cache_read_above);
        if fast { cost * self.fast } else { cost }
    }
}

/// The downloaded list, read into rates.
#[derive(Debug, Default)]
pub struct Prices {
    rates: HashMap<String, Rate>,
}

impl Prices {
    /// Read LiteLLM's JSON. Entries without both an input and an output rate
    /// (image and embedding models, the `sample_spec` header) are left out.
    pub fn parse(text: &str) -> Result<Prices, String> {
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(text).map_err(|e| format!("price list does not parse: {e}"))?;
        let mut rates = HashMap::new();
        for (key, e) in raw {
            let num = |k: &str| e.get(k).and_then(serde_json::Value::as_f64);
            let (Some(input), Some(output)) =
                (num("input_cost_per_token"), num("output_cost_per_token"))
            else {
                continue;
            };
            let fast = e
                .get("provider_specific_entry")
                .and_then(|p| p.get("fast"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0);
            rates.insert(
                key,
                Rate {
                    input,
                    output,
                    cache_write: num("cache_creation_input_token_cost").unwrap_or(input * 1.25),
                    cache_read: num("cache_read_input_token_cost").unwrap_or(input * 0.1),
                    input_above: num("input_cost_per_token_above_200k_tokens"),
                    output_above: num("output_cost_per_token_above_200k_tokens"),
                    cache_write_above: num("cache_creation_input_token_cost_above_200k_tokens"),
                    cache_read_above: num("cache_read_input_token_cost_above_200k_tokens"),
                    fast,
                },
            );
        }
        if rates.is_empty() {
            return Err("price list names no models".into());
        }
        Ok(Prices { rates })
    }

    /// The rate for `model`: its own entry, else the longest entry that names
    /// it or that it names, at word boundaries (`claude-opus-5-20261001` finds
    /// `claude-opus-5`, `qwen/qwen3` finds `groq/qwen/qwen3`), never one that
    /// only shares a prefix with a different version (`claude-opus-5` is not
    /// `claude-opus-5-5`). Equal lengths go to the alphabetically first key.
    pub fn find(&self, model: &str) -> Option<&Rate> {
        if let Some(r) = self.rates.get(model) {
            return Some(r);
        }
        let normal = normalize(model);
        self.rates
            .iter()
            .filter(|(key, _)| key_matches(key, model, &normal))
            .max_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| b.cmp(a)))
            .map(|(_, r)| r)
    }
}

fn normalize(s: &str) -> String {
    s.replace(['.', '@'], "-")
}

fn key_matches(key: &str, model: &str, normal_model: &str) -> bool {
    if contains_at_boundary(model, key) || contains_at_boundary(key, model) {
        return true;
    }
    let normal_key = normalize(key);
    contains_at_boundary(normal_model, &normal_key)
        || contains_at_boundary(&normal_key, normal_model)
}

fn boundary(b: u8) -> bool {
    !b.is_ascii_alphanumeric()
}

/// Whether `key` occurs in `value` with a non-alphanumeric byte (or nothing)
/// on each side, and is not followed by a further version number: `-5` after
/// `claude-opus-5` makes a different model, while an eight-digit date does not.
fn contains_at_boundary(value: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    value.match_indices(key).any(|(i, _)| {
        let before_ok = i == 0 || boundary(value.as_bytes()[i - 1]);
        let suffix = &value[i + key.len()..];
        let after_ok = match suffix.as_bytes().first() {
            None => true,
            Some(&b) if !boundary(b) => false,
            Some(_) => !continues_version(key, suffix),
        };
        before_ok && after_ok
    })
}

fn continues_version(key: &str, suffix: &str) -> bool {
    if !key.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    if !matches!(suffix.as_bytes().first(), Some(b'-' | b'.')) {
        return false;
    }
    let rest = &suffix.as_bytes()[1..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let after = rest.get(digits).copied();
    !(digits == 8 && after.is_none_or(boundary))
}

pub fn path() -> PathBuf {
    store::cache_dir().join("prices.json")
}

/// `CCMONETA_PRICES_URL` replaces LiteLLM's address; the tests point it at a
/// local `file://` list, which curl reads like any other.
fn url() -> String {
    std::env::var("CCMONETA_PRICES_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| LITELLM_URL.to_string())
}

fn age(path: &std::path::Path) -> Option<i64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(store::now() - secs)
}

/// The prices, downloaded again when the cached copy is a day old or missing.
///
/// A download that fails, or that does not parse, leaves the cached copy in
/// use; only with no usable copy at all is that an error. An unchanged list
/// is not written again, only touched, so a day's check costs no disk writes
/// beyond the file's time stamp.
pub fn load() -> Result<Prices, String> {
    let path = path();
    let cached = std::fs::read_to_string(&path).ok();
    let fresh = age(&path).is_some_and(|a| (0..MAX_AGE).contains(&a));
    if let (Some(text), true) = (&cached, fresh)
        && let Ok(p) = Prices::parse(text)
    {
        return Ok(p);
    }
    let fetched = crate::health::curl_text(&url()).and_then(|t| Prices::parse(&t).map(|p| (t, p)));
    match fetched {
        Ok((text, prices)) => {
            if cached.as_deref() == Some(text.as_str()) {
                let _ = std::fs::File::options()
                    .append(true)
                    .open(&path)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()));
            } else {
                store::write_text(&path, &text)
                    .map_err(|e| format!("writing {}: {e}", path.display()))?;
            }
            Ok(prices)
        }
        Err(e) => match cached.as_deref().map(Prices::parse) {
            Some(Ok(p)) => {
                store::log(&format!(
                    "price list not refreshed, using the cached one: {e}"
                ));
                Ok(p)
            }
            _ => Err(format!("no model prices: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"{
        "sample_spec": {"input_cost_per_token": "price"},
        "claude-opus-5": {"input_cost_per_token": 5e-06, "output_cost_per_token": 2.5e-05,
            "cache_creation_input_token_cost": 6.25e-06, "cache_read_input_token_cost": 5e-07,
            "provider_specific_entry": {"us": 1.1, "fast": 2.0}},
        "claude-opus-5-5": {"input_cost_per_token": 4e-06, "output_cost_per_token": 2e-05},
        "claude-sonnet-4": {"input_cost_per_token": 3e-06, "output_cost_per_token": 1.5e-05,
            "input_cost_per_token_above_200k_tokens": 6e-06,
            "cache_read_input_token_cost_above_200k_tokens": 6e-07},
        "groq/qwen/qwen3-27b": {"input_cost_per_token": 1e-07, "output_cost_per_token": 2e-07},
        "dall-e-3": {"output_cost_per_pixel": 1e-08}
    }"#;

    fn prices() -> Prices {
        Prices::parse(LIST).unwrap()
    }

    #[test]
    fn a_model_finds_its_own_entry_before_any_other() {
        let p = prices();
        assert_eq!(p.find("claude-opus-5-5").unwrap().input, 4e-06);
        assert_eq!(p.find("claude-opus-5").unwrap().input, 5e-06);
    }

    #[test]
    fn a_dated_or_prefixed_name_finds_its_model_but_not_a_newer_version() {
        let p = prices();
        assert_eq!(p.find("claude-opus-5-20261001").unwrap().input, 5e-06);
        assert_eq!(p.find("anthropic/claude-opus-5").unwrap().input, 5e-06);
        assert_eq!(p.find("qwen/qwen3-27b").unwrap().input, 1e-07);
        assert_eq!(p.find("qwen/qwen3.27b").unwrap().input, 1e-07);
        // "-6" after "claude-opus-5" is another version, not a date.
        assert!(p.find("claude-opus-5-6").is_none());
        assert!(p.find("poolside/laguna:free").is_none());
        assert!(
            p.find("dall-e-3").is_none(),
            "no token rates, not a model here"
        );
    }

    #[test]
    fn missing_cache_rates_default_as_in_ccusage() {
        let r = *prices().find("claude-opus-5-5").unwrap();
        assert!((r.cache_write - 5e-06).abs() < 1e-18);
        assert!((r.cache_read - 4e-07).abs() < 1e-18);
        assert_eq!(r.fast, 1.0);
    }

    #[test]
    fn the_formula_prices_every_bucket() {
        let r = *prices().find("claude-opus-5").unwrap();
        let t = Tokens {
            input: 1_000,
            output: 100,
            cache_5m: 2_000,
            cache_1h: 3_000,
            cache_read: 10_000,
        };
        let want = 1_000.0 * 5e-06
            + 100.0 * 2.5e-05
            + 2_000.0 * 6.25e-06
            + 3_000.0 * 1e-05
            + 10_000.0 * 5e-07;
        assert!((r.cost(&t, false) - want).abs() < 1e-12);
        assert!((r.cost(&t, true) - 2.0 * want).abs() < 1e-12);
    }

    #[test]
    fn tokens_past_200k_in_one_request_take_the_higher_rate() {
        let r = *prices().find("claude-sonnet-4").unwrap();
        let t = Tokens {
            input: 250_000,
            cache_read: 300_000,
            cache_1h: 210_000,
            ..Default::default()
        };
        let want = 200_000.0 * 3e-06
            + 50_000.0 * 6e-06
            + 200_000.0 * 3e-07
            + 100_000.0 * 6e-07
            + 200_000.0 * 6e-06
            + 10_000.0 * 12e-06;
        assert!(
            (r.cost(&t, false) - want).abs() < 1e-9,
            "{}",
            r.cost(&t, false)
        );
        // Without above-200k rates the count is priced flat.
        let flat = *prices().find("claude-opus-5-5").unwrap();
        let t = Tokens {
            input: 300_000,
            ..Default::default()
        };
        assert!((flat.cost(&t, false) - 1.2).abs() < 1e-9);
    }

    #[test]
    fn a_list_without_models_is_refused() {
        assert!(Prices::parse("{}").is_err());
        assert!(Prices::parse("<html>").is_err());
    }
}
