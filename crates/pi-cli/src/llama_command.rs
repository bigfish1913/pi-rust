//! LLaMA.cpp integration command.
//!
//! Provides CLI commands for managing local LLM inference using llama.cpp server.

use std::path::PathBuf;
use rpi_ai::providers::llama_cpp::{LlamaCppConfig, LlamaCppModelManager};

/// LLaMA subcommand
#[derive(Debug)]
pub enum LlamaCommand {
    /// Start llama.cpp server
    Start {
        model: PathBuf,
        port: u16,
        ctx_size: usize,
        threads: Option<usize>,
    },
    /// Stop llama.cpp server
    Stop,
    /// List available models
    List {
        server_url: String,
    },
    /// Show server status
    Status {
        server_url: String,
    },
    /// Download a model
    Download {
        model: String,
        output: Option<PathBuf>,
    },
}

/// Parse llama subcommand from args
pub fn parse_llama_command(args: &[String]) -> Result<LlamaCommand, String> {
    if args.is_empty() {
        return Err("Usage: rpi llama <start|stop|list|status|download> [options]".to_string());
    }
    
    let subcommand = &args[0];
    let sub_args = &args[1..];
    
    match subcommand.as_str() {
        "start" => parse_start_command(sub_args),
        "stop" => Ok(LlamaCommand::Stop),
        "list" => parse_list_command(sub_args),
        "status" => parse_status_command(sub_args),
        "download" => parse_download_command(sub_args),
        _ => Err(format!("Unknown llama subcommand: {}", subcommand)),
    }
}

fn parse_start_command(args: &[String]) -> Result<LlamaCommand, String> {
    let mut model = None;
    let mut port = 8080u16;
    let mut ctx_size = 2048usize;
    let mut threads = None;
    
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" | "-m" => {
                i += 1;
                if i >= args.len() {
                    return Err("--model requires a value".to_string());
                }
                model = Some(PathBuf::from(&args[i]));
            }
            "--port" | "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err("--port requires a value".to_string());
                }
                port = args[i].parse().map_err(|_| "Invalid port number")?;
            }
            "--ctx-size" | "-c" => {
                i += 1;
                if i >= args.len() {
                    return Err("--ctx-size requires a value".to_string());
                }
                ctx_size = args[i].parse().map_err(|_| "Invalid context size")?;
            }
            "--threads" | "-t" => {
                i += 1;
                if i >= args.len() {
                    return Err("--threads requires a value".to_string());
                }
                threads = Some(args[i].parse().map_err(|_| "Invalid thread count")?);
            }
            _ => {
                return Err(format!("Unknown option: {}", args[i]));
            }
        }
        i += 1;
    }
    
    let model = model.ok_or("--model is required")?;
    
    Ok(LlamaCommand::Start {
        model,
        port,
        ctx_size,
        threads,
    })
}

fn parse_list_command(args: &[String]) -> Result<LlamaCommand, String> {
    let mut server_url = "http://localhost:8080".to_string();
    
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" | "-s" => {
                i += 1;
                if i >= args.len() {
                    return Err("--server requires a value".to_string());
                }
                server_url = args[i].clone();
            }
            _ => {
                return Err(format!("Unknown option: {}", args[i]));
            }
        }
        i += 1;
    }
    
    Ok(LlamaCommand::List { server_url })
}

fn parse_status_command(args: &[String]) -> Result<LlamaCommand, String> {
    let mut server_url = "http://localhost:8080".to_string();
    
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" | "-s" => {
                i += 1;
                if i >= args.len() {
                    return Err("--server requires a value".to_string());
                }
                server_url = args[i].clone();
            }
            _ => {
                return Err(format!("Unknown option: {}", args[i]));
            }
        }
        i += 1;
    }
    
    Ok(LlamaCommand::Status { server_url })
}

fn parse_download_command(args: &[String]) -> Result<LlamaCommand, String> {
    if args.is_empty() {
        return Err("Usage: rpi llama download <model> [--output <path>]".to_string());
    }
    
    let model = args[0].clone();
    let mut output = None;
    
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--output" | "-o" => {
                i += 1;
                if i >= args.len() {
                    return Err("--output requires a value".to_string());
                }
                output = Some(PathBuf::from(&args[i]));
            }
            _ => {
                return Err(format!("Unknown option: {}", args[i]));
            }
        }
        i += 1;
    }
    
    Ok(LlamaCommand::Download { model, output })
}

/// Execute llama command
pub async fn run_llama_command(cmd: LlamaCommand) -> Result<(), String> {
    match cmd {
        LlamaCommand::Start { model, port, ctx_size, threads } => {
            start_server(model, port, ctx_size, threads).await
        }
        LlamaCommand::Stop => {
            stop_server().await
        }
        LlamaCommand::List { server_url } => {
            list_models(server_url).await
        }
        LlamaCommand::Status { server_url } => {
            show_status(server_url).await
        }
        LlamaCommand::Download { model, output } => {
            download_model(model, output).await
        }
    }
}

async fn start_server(
    model: PathBuf,
    port: u16,
    ctx_size: usize,
    threads: Option<usize>,
) -> Result<(), String> {
    println!("Starting llama.cpp server...");
    println!("  Model: {}", model.display());
    println!("  Port: {}", port);
    println!("  Context size: {}", ctx_size);
    
    if let Some(threads) = threads {
        println!("  Threads: {}", threads);
    }
    
    // Check if model file exists
    if !model.exists() {
        return Err(format!("Model file not found: {}", model.display()));
    }
    
    // Build llama-server command
    let mut cmd = tokio::process::Command::new("llama-server");
    cmd.arg("--model").arg(&model)
       .arg("--port").arg(port.to_string())
       .arg("--ctx-size").arg(ctx_size.to_string());
    
    if let Some(threads) = threads {
        cmd.arg("--threads").arg(threads.to_string());
    }
    
    // Start server in background
    let child = cmd.spawn().map_err(|e| format!("Failed to start llama-server: {}", e))?;
    
    println!("Server started (PID: {:?})", child.id());
    println!("Server URL: http://localhost:{}", port);
    
    // Wait a bit for server to initialize
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Check if server is responding
    let config = LlamaCppConfig {
        server_url: format!("http://localhost:{}", port),
        timeout: std::time::Duration::from_secs(5),
        api_key: None,
    };
    
    let manager = LlamaCppModelManager::new(config);
    match manager.is_healthy().await {
        Ok(true) => {
            println!("Server is ready!");
            Ok(())
        }
        Ok(false) => {
            println!("Warning: Server started but not responding yet");
            Ok(())
        }
        Err(e) => {
            println!("Warning: Could not verify server status: {}", e);
            Ok(())
        }
    }
}

async fn stop_server() -> Result<(), String> {
    println!("Stopping llama.cpp server...");
    
    // Try to find and kill llama-server process
    #[cfg(unix)]
    {
        let output = tokio::process::Command::new("pkill")
            .arg("-f")
            .arg("llama-server")
            .output()
            .await
            .map_err(|e| format!("Failed to stop server: {}", e))?;
        
        if output.status.success() {
            println!("Server stopped");
        } else {
            println!("No running server found");
        }
    }
    
    #[cfg(windows)]
    {
        let output = tokio::process::Command::new("taskkill")
            .arg("/F")
            .arg("/IM")
            .arg("llama-server.exe")
            .output()
            .await
            .map_err(|e| format!("Failed to stop server: {}", e))?;
        
        if output.status.success() {
            println!("Server stopped");
        } else {
            println!("No running server found");
        }
    }
    
    Ok(())
}

async fn list_models(server_url: String) -> Result<(), String> {
    let config = LlamaCppConfig {
        server_url,
        timeout: std::time::Duration::from_secs(10),
        api_key: None,
    };
    
    let manager = LlamaCppModelManager::new(config);
    
    println!("Fetching model list...");
    
    match manager.list_models().await {
        Ok(models) => {
            if models.is_empty() {
                println!("No models available");
            } else {
                println!("Available models:");
                for model in models {
                    println!("  - {}", model);
                }
            }
            Ok(())
        }
        Err(e) => {
            Err(format!("Failed to list models: {}", e))
        }
    }
}

async fn show_status(server_url: String) -> Result<(), String> {
    let config = LlamaCppConfig {
        server_url: server_url.clone(),
        timeout: std::time::Duration::from_secs(5),
        api_key: None,
    };
    
    let manager = LlamaCppModelManager::new(config);
    
    println!("Checking server status at {}...", server_url);
    
    match manager.is_healthy().await {
        Ok(true) => {
            println!("✓ Server is running and healthy");
            
            // Try to get model info
            if let Ok(models) = manager.list_models().await {
                if !models.is_empty() {
                    println!("\nLoaded models:");
                    for model in models {
                        println!("  - {}", model);
                    }
                }
            }
            
            Ok(())
        }
        Ok(false) => {
            println!("✗ Server is not responding");
            Ok(())
        }
        Err(e) => {
            println!("✗ Failed to check server status: {}", e);
            Ok(())
        }
    }
}

async fn download_model(model: String, output: Option<PathBuf>) -> Result<(), String> {
    println!("Downloading model: {}", model);
    
    // Determine output path
    let output_path = output.unwrap_or_else(|| {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".cache").join("llama-cpp").join("models")
    });
    
    // Create output directory
    std::fs::create_dir_all(&output_path)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;
    
    println!("Output directory: {}", output_path.display());
    
    // Check if it's a URL
    if model.starts_with("http://") || model.starts_with("https://") {
        println!("Downloading from URL...");
        
        let output_file = output_path.join("model.gguf");
        
        let response = reqwest::get(&model)
            .await
            .map_err(|e| format!("Failed to download model: {}", e))?;
        
        if !response.status().is_success() {
            return Err(format!("Download failed with status: {}", response.status()));
        }
        
        let bytes = response.bytes()
            .await
            .map_err(|e| format!("Failed to read response: {}", e))?;
        
        std::fs::write(&output_file, bytes)
            .map_err(|e| format!("Failed to write model file: {}", e))?;
        
        println!("Model downloaded to: {}", output_file.display());
    } else {
        // Assume it's a Hugging Face model ID
        println!("Downloading from Hugging Face...");
        println!("Note: Hugging Face download not yet implemented");
        println!("Please download manually from: https://huggingface.co/{}", model);
    }
    
    Ok(())
}
