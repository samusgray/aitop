pub struct ModelInfo {
    pub context_window: u64,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

pub fn lookup(model: &str) -> ModelInfo {
    let model = model.to_ascii_lowercase();
    if model.contains("opus") {
        ModelInfo {
            context_window: 200_000,
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
        }
    } else if model.contains("sonnet") {
        ModelInfo {
            context_window: 200_000,
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        }
    } else if model.contains("haiku") {
        ModelInfo {
            context_window: 200_000,
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
        }
    } else {
        ModelInfo {
            context_window: 258_400,
            input_per_mtok: 2.5,
            output_per_mtok: 10.0,
        }
    }
}

pub fn estimate_cost(input: u64, output: u64, model: &str) -> f64 {
    let info = lookup(model);
    ((input as f64 * info.input_per_mtok) + (output as f64 * info.output_per_mtok)) / 1_000_000.0
}

pub fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub fn short_model(model: &str) -> String {
    let body = model.trim().strip_prefix("claude-").unwrap_or(model.trim());
    let parts = body.split('-').collect::<Vec<_>>();
    if parts.len() >= 3
        && parts[parts.len() - 2].parse::<u32>().is_ok()
        && parts[parts.len() - 1].parse::<u32>().is_ok()
    {
        let head = parts[..parts.len() - 2].join("-");
        return format!(
            "{head}-{}.{}",
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        );
    }
    body.to_string()
}
