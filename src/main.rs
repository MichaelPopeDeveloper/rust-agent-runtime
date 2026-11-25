use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};
use std::env;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let anthropic_api_key = env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");

    let client = Client::new();

    llm_api_request(client, anthropic_api_key).await;
}

async fn llm_api_request(
    client: reqwest::Client,
    apiKey: String,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("2. seinding request to Anthropic...");

    let body = json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 1000,
        "messages": [
        {"role": "user", "content": "Hello, Claude!"}
        ]
    });

    let llm_response = client
        .post("https://anthropic.com")
        .header(AUTHORIZATION, apiKey)
        .json(&body)
        .send()
        .await?;

    println!("3. Status: {}", llm_response.status());

    let raw_text = llm_response.text().await?;
    // println!("4. Raw response: {}", raw_text);

    let _parsed: Value = serde_json::from_str(&raw_text)?;
    println!("Full JSON: \n{:#?}", _parsed["content"][0]["text"].as_str());
    Ok(())
}
