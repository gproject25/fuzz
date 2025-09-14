use crate::program::Program;

use self::prompt::Prompt;

pub mod openai;
pub mod prompt;
pub mod http;

pub trait Handler {
    /// generate programs via a formatted prompt
    fn generate(&self, prompt: &Prompt) -> eyre::Result<Vec<Program>>;
    fn generate_json(&self, prompt: String) -> eyre::Result<serde_json::Value>;
}
