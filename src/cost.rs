//! Rough cost estimation. Prices are USD per million tokens and are *estimates*
//! kept here as editable constants — adjust them to match current Anthropic
//! pricing for your plan. Cache writes bill above base input; cache reads far
//! below it.

use crate::session::Usage;

#[derive(Clone, Copy)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// Best-effort pricing lookup by model id substring. Defaults to Opus-tier.
pub fn pricing_for(model: Option<&str>) -> Pricing {
    let m = model.unwrap_or("").to_lowercase();
    if m.contains("haiku") {
        Pricing {
            input: 0.80,
            output: 4.0,
            cache_write: 1.0,
            cache_read: 0.08,
        }
    } else if m.contains("sonnet") {
        Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else {
        // opus / fable / unknown -> opus-tier estimate
        Pricing {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        }
    }
}

/// Estimated USD cost for the accumulated usage of a session.
pub fn estimate(usage: &Usage, model: Option<&str>) -> f64 {
    let p = pricing_for(model);
    let per = |tokens: u64, rate: f64| (tokens as f64) / 1_000_000.0 * rate;
    per(usage.input, p.input)
        + per(usage.output, p.output)
        + per(usage.cache_creation, p.cache_write)
        + per(usage.cache_read, p.cache_read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_creation: u64, cache_read: u64) -> Usage {
        Usage {
            input,
            output,
            cache_creation,
            cache_read,
        }
    }

    #[test]
    fn estimate_uses_the_model_tier() {
        let u = usage(1_000_000, 1_000_000, 0, 0);
        assert_eq!(estimate(&u, Some("claude-haiku-4-5")), 0.80 + 4.0);
        assert_eq!(estimate(&u, Some("claude-sonnet-5")), 3.0 + 15.0);
        // Unknown / fable / opus all price at the opus tier.
        assert_eq!(estimate(&u, Some("claude-fable-5")), 15.0 + 75.0);
        assert_eq!(estimate(&u, None), 15.0 + 75.0);
    }

    #[test]
    fn estimate_counts_cache_tokens() {
        let u = usage(0, 0, 2_000_000, 10_000_000);
        let est = estimate(&u, Some("claude-sonnet-5"));
        assert!((est - (2.0 * 3.75 + 10.0 * 0.30)).abs() < 1e-9);
    }

    #[test]
    fn session_costs_sum_across_models() {
        // The header total is a plain sum of per-session estimates.
        let a = estimate(&usage(500_000, 100_000, 0, 0), Some("claude-haiku-4-5"));
        let b = estimate(&usage(500_000, 100_000, 0, 0), Some("claude-opus-4-8"));
        assert!(a > 0.0 && b > a);
        assert!((a + b) > b);
    }
}
