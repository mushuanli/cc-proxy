use proxy_common::{PriceRates, TaskUsage};

/// Error type for billing calculations.
#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("cost overflow: total exceeds i64 range")]
    CostOverflow,
}

/// Calculate cost in micro-USD given token usage and pricing rates.
///
/// All inputs are integers to avoid floating-point drift:
/// - `usage` fields are raw token counts
/// - `rates` fields are micro-USD per 1,000,000 tokens
/// - result is micro-USD (1 USD = 1,000,000 micro-USD)
///
/// Performs intermediate arithmetic in i128 to prevent overflow,
/// then rounds half-up and narrows to i64.
pub fn calculate_cost_microusd(usage: &TaskUsage, rates: &PriceRates) -> Result<i64, BillingError> {
    let total = i128::from(usage.input_tokens) * i128::from(rates.input_microusd)
        + i128::from(usage.output_tokens) * i128::from(rates.output_microusd)
        + i128::from(usage.cache_creation_tokens) * i128::from(rates.cache_write_microusd)
        + i128::from(usage.cache_read_tokens) * i128::from(rates.cache_read_microusd);

    let rounded = (total + 500_000) / 1_000_000;

    i64::try_from(rounded).map_err(|_| BillingError::CostOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_usage_zero_cost() {
        let usage = TaskUsage::default();
        let rates = PriceRates::default();
        assert_eq!(calculate_cost_microusd(&usage, &rates).unwrap(), 0);
    }

    #[test]
    fn basic_calculation() {
        let usage = TaskUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
        };
        let rates = PriceRates {
            input_microusd: 3_000_000,
            output_microusd: 15_000_000,
            cache_write_microusd: 3_750_000,
            cache_read_microusd: 300_000,
        };
        assert_eq!(calculate_cost_microusd(&usage, &rates).unwrap(), 10_500_000);
    }

    #[test]
    fn rounding_works() {
        let usage = TaskUsage {
            input_tokens: 1,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
        };
        let rates = PriceRates {
            input_microusd: 3_000_000,
            output_microusd: 0,
            cache_write_microusd: 0,
            cache_read_microusd: 0,
        };
        assert_eq!(calculate_cost_microusd(&usage, &rates).unwrap(), 3);
    }
}
