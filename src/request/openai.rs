use std::{process::Child, time::Duration};
use crate::{
    config::{self, get_config, get_openai_proxy},
    is_critical_err,
    program::Program,
    FuzzerError,
    deopt::Deopt, 
    analysis::header as headers,
};
use async_openai::{
    config::OpenAIConfig, types::{
        ChatCompletionRequestMessage, CreateChatCompletionRequest, CreateChatCompletionRequestArgs, CreateChatCompletionResponse,  ResponseFormatJsonSchema,ResponseFormat, ChatCompletionRequestUserMessageArgs,
    }, Client
};
use eyre::Result;
use once_cell::sync::OnceCell;
use futures::future::join_all;

use serde_json::{json, to_value, Value};

use super::Handler;

/// Token使用统计结构
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    pub fn new(prompt_tokens: u32, completion_tokens: u32, total_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }
    }
    
    pub fn from_response(response: &CreateChatCompletionResponse) -> Self {
        if let Some(usage) = &response.usage {
            Self {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
            }
        } else {
            Self::default()
        }
    }
    
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

pub struct OpenAIHanler {
    _child: Option<Child>,
    rt: tokio::runtime::Runtime,
}

impl Default for OpenAIHanler {
    fn default() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|_| panic!("Unable to build the openai runtime."));
        Self { _child: None, rt }
    }
}

impl Handler for OpenAIHanler {
    /// Generate `SAMPLE_N` programs by chatting with instructions.
    fn generate(&self, prompt: &super::prompt::Prompt) -> eyre::Result<Vec<Program>> {
        let start = std::time::Instant::now();
        let chat_msgs = prompt.to_chatgpt_message();
        
        
       /* println!("===== LLM Prompt Start =====");
	for (i, msg) in chat_msgs.iter().enumerate() {
	    // Use Debug print for entire message struct
	    println!("Message {}:\n{}", i, serde_json::to_string_pretty(&msg)?);
	}
	println!("===== LLM Prompt End =====");*/
	
        let mut futures = Vec::new();
        for _ in 0..get_config().n_sample {
            let future = generate_program_by_chat(chat_msgs.clone());
            futures.push(future);
        }
        let results = self.rt.block_on(join_all(futures));
        
        let mut programs = Vec::new();
        let mut total_usage = TokenUsage::default();
        
        for result in results {
            let (program, usage) = result?;
            programs.push(program);
            total_usage.add(&usage);
        }
        
        let elapsed = start.elapsed();
        log::info!("OpenAI Generate time: {}s", elapsed.as_secs());
        log::info!("OpenAI Token Usage - Prompt: {}, Completion: {}, Total: {}", 
                  total_usage.prompt_tokens, 
                  total_usage.completion_tokens, 
                  total_usage.total_tokens);
        
        Ok(programs)
    }
    
    fn generate_json(&self, prompt: String, deopt: &Deopt) -> eyre::Result<serde_json::Value> {
        let mut files = headers::get_include_sys_headers(deopt).clone();
        files.extend(headers::get_include_lib_headers(deopt)?);
        
        let mut allfiles = Vec::new();
    	for header in &files {
        	let path = headers::resolve_lib_header(deopt, header)?;
        	allfiles.push(path.to_string_lossy().to_string());
    	}
    	
    	//add document
    	let docs_path = deopt.get_library_build_dir()?;
    	for candidate in &["README.md", "README.txt", "README"] {
    		let path = docs_path.join(candidate);
    		if path.exists() {
        		allfiles.push(path.to_string_lossy().to_string());
        	}
    	}	
        
        self.rt.block_on(async {
        	let (json, _usage) = generate_json_by_chat(prompt,Some(allfiles)).await?;
        	Ok(json)
    	})
    }
}

/// Get the OpenAI interface client.
fn get_client() -> Result<&'static Client<OpenAIConfig>> {
    // read OpenAI API key form the env var (OPENAI_API_KEY).
    pub static CLIENT: OnceCell<Client<OpenAIConfig>> = OnceCell::new();
    let client = CLIENT.get_or_init(|| {
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .build()
            .unwrap();
        let openai_config = if let Some(proxy) = get_openai_proxy() {
            OpenAIConfig::default().with_api_base(proxy)
        } else {
            OpenAIConfig::new()
        };
        let client = Client::with_config(openai_config);
        let client = client.with_http_client(http_client);
        client
    });
    Ok(client)
}

/// Create a request for a chat prompt
fn create_chat_request(
    msgs: Vec<ChatCompletionRequestMessage>,
    stop: Option<String>,
) -> Result<CreateChatCompletionRequest> {
    let mut binding = CreateChatCompletionRequestArgs::default();
    let binding = binding.model(config::get_openai_model_name());

    let mut request = binding
        .messages(msgs)
        .temperature(config::get_config().temperature);
    if let Some(stop) = stop {
        request = request.stop(stop);
    }
    let request = request.build()?;
    Ok(request)
}

fn create_structured_request(
    msg: String,
    stop: Option<String>,
    files: Option<Vec<String>>,
) -> Result<CreateChatCompletionRequest> {
    let mut binding = CreateChatCompletionRequestArgs::default();
    let binding = binding.model(config::get_openai_model_name());

    let schema = json!({
	    "type": "object",
	    "properties": {
		"APIs": {
		    "type": "array",
		    "description": "api of the library",
		    "items": {
		        "type": "object",
		        "properties": {
		            "name": { "type": "string", "description": "Function name of the API" },
		            "arg_ownership_info": { 
		                "type": "array",
		                "description": "Information about responsibility of freeing, if caller keeps ownership or not.",
		                "items": { 
		                    "enum": ["Caller keeps ownership", "Caller loses ownership", "None"], 
		                    "type": "string" 
		                }
		            },
		            "ret_ownership_info": { 
		                "enum": ["Caller owns", "Library owns", "None"], 
		                "type": "string", 
		                "description": "Information about responsibility of freeing, if caller has ownership or not." 
		            },
		            "func_info": { "type": "string", "description": "Other useful information for fuzzing harness generation (ex: must-follow how-to-use, other function which should be called before this function, etc)" }
		        },
		        "required": ["name", "arg_ownership_info","ret_ownership_info", "func_info"],
		        "additionalProperties": false
		    }
		},
		"library_boilerplate": {
		    "type": "string",
		    "description": "Must-follow boilerplate, library-specific error handeling, and other informations needed for making fuzzing harness."
		}
	    },
	    "required": ["APIs", "library_boilerplate"],
	    "additionalProperties": false
    });
    
    let mut full_msg = msg;
    if let Some(paths) = files {
        let mut header_files = Vec::new();
        let mut doc_files = Vec::new();
        
        for path in &paths {
        	if path.to_lowercase().contains("readme") {
            		doc_files.push(path);
        	} else {
            		header_files.push(path);
        	}
    	}
	for path in &header_files {
	    match std::fs::read_to_string(path) {
		Ok(text) => {
		    full_msg.push_str(&format!("\n--- Header File ---\n"));
		    full_msg.push_str(&text);
		}
		Err(e) => log::warn!("Could not read {}: {}", path, e),
	    }
	}

	for path in &doc_files {
	    match std::fs::read_to_string(path) {
		Ok(text) => {
		    full_msg.push_str(&format!("\n--- Documentation File  ---\n"));
		    full_msg.push_str(&text);
		}
		Err(e) => log::warn!("Could not read {}: {}", path, e),
	    }
	}
    }
    
    let user_msg = ChatCompletionRequestUserMessageArgs::default()
        .content(full_msg)
        .build()?
        .into();
    
    let mut request = binding
        .messages(vec![user_msg])
        .temperature(config::get_config().temperature)
        .response_format(ResponseFormat::JsonSchema {
            json_schema: ResponseFormatJsonSchema {
                schema: Some(schema),
                description: Some("Extract structured API info for fuzzing harness".into()),
                name: "fuzzing_harness_gen".into(),
                strict: Some(true),
            }
        });
    
    if let Some(stop) = stop {
        request = request.stop(stop);
    }
    let request = request.build()?;

    Ok(request)
}


/// Get a response for a chat request
async fn get_chat_response(
    request: CreateChatCompletionRequest,
) -> Result<CreateChatCompletionResponse> {
    let client = get_client().unwrap();
    for _retry in 0..config::RETRY_N {
        let response = client
            .chat()
            .create(request.clone())
            .await
            .map_err(eyre::Report::new);
        match is_critical_err(&response) {
            crate::Critical::Normal => {
                let response = response?;
                return Ok(response);
            }
            crate::Critical::NonCritical => {
                continue;
            }
            crate::Critical::Critical => return Err(response.err().unwrap()),
        }
    }
    Err(FuzzerError::RetryError(format!("{request:?}"), config::RETRY_N).into())
}

pub async fn generate_json_by_chat(
    prompt: String,
    files: Option<Vec<String>>,
) -> Result<(serde_json::Value, TokenUsage)> {
    
    let request = create_structured_request(prompt, None, files)?;
    let respond = get_chat_response(request).await?;
    
    let usage = TokenUsage::from_response(&respond);
    let choice = respond.choices.first().unwrap();
    let content = choice.message.content.as_ref().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(content)?;
    Ok((parsed, usage))
}

pub async fn generate_program_by_chat(
    chat_msgs: Vec<ChatCompletionRequestMessage>,
) -> Result<(Program, TokenUsage)> {

    let request = create_chat_request(chat_msgs, None)?;
    let respond = get_chat_response(request).await?;
    
    let usage = TokenUsage::from_response(&respond);
    let choice = respond.choices.first().unwrap();
    let content = choice.message.content.as_ref().unwrap();
    let content = strip_code_wrapper(&content);
    let program = Program::new(&content);
    Ok((program, usage))
}


fn strip_code_prefix<'a>(input: &'a str, pat: &str) -> &'a str {
    let pat = String::from_iter(["```", pat]);
    if input.starts_with(&pat) {
        if let Some(p) = input.strip_prefix(&pat) {
            return p;
        }
    }
    input
}

/// strip the code wrapper that ChatGPT generated with code.
fn strip_code_wrapper(input: &str) -> String {
    let mut input = input.trim();
    let mut event = "";
    if let Some(idx) = input.find("```") {
        event = &input[..idx];
        input = &input[idx..];
    }
    let input = strip_code_prefix(input, "cpp");
    let input = strip_code_prefix(input, "CPP");
    let input = strip_code_prefix(input, "C++");
    let input = strip_code_prefix(input, "c++");
    let input = strip_code_prefix(input, "c");
    let input = strip_code_prefix(input, "C");
    let input = strip_code_prefix(input, "\n");
    if let Some(idx) = input.rfind("```") {
        let input = &input[..idx];
        let input = ["/*", event, "*/\n", input].concat();
        return input;
    }
    ["/*", event, "*/\n", input].concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs};
    use eyre::Result;

    #[tokio::test]  // async test
    async fn test_generate_json() -> Result<()> {
        dotenv::dotenv().ok(); // make sure OPENAI_API_KEY is loaded
        config::init_openai_env();
        println!("API_KEY: {:?}", std::env::var("OPENAI_API_KEY"));
	println!("MODEL: {:?}", std::env::var("OPENAI_MODEL_NAME"));

        let prompt = "Explain Rust's ownership system in JSON format.".to_string();

        // call your function
        let (json, usage) = generate_json_by_chat(prompt, None).await?;

        println!("JSON response:\n{}", serde_json::to_string_pretty(&json)?);
        println!("Token usage: {:?}", usage);

        Ok(())
    }
}
