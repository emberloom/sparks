use crate::config::{AgentConfig, Config};
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::llm::{self, Message, OllamaClient};
use crate::memory::MemoryStore;
use crate::strategy::TaskContract;

pub struct Manager {
    llm: OllamaClient,
    executor: Executor,
    agents: Vec<AgentConfig>,
    memory: std::sync::Arc<MemoryStore>,
}

impl Manager {
    pub fn new(config: &Config, llm: OllamaClient, memory: std::sync::Arc<MemoryStore>) -> Self {
        let executor = Executor::new(
            config.docker.clone(),
            config.manager.max_steps,
            config.manager.sensitive_patterns.clone(),
        );

        Self {
            llm,
            executor,
            agents: config.agents.clone(),
            memory,
        }
    }

    /// Handle a user message: classify, delegate or answer directly
    pub async fn handle(&self, user_input: &str) -> Result<String> {
        // Load relevant memories for context
        let memories = self.memory.search(user_input).unwrap_or_default();
        let memory_context = if memories.is_empty() {
            String::new()
        } else {
            let items: Vec<String> = memories.iter()
                .map(|m| format!("- [{}] {}", m.category, m.content))
                .collect();
            format!("\n\nRelevant memories:\n{}", items.join("\n"))
        };

        // Classify the request
        let classification = self.classify(user_input, &memory_context).await?;

        match classification {
            Classification::Simple(answer) => Ok(answer),
            Classification::Complex { agent_name, goal, context } => {
                let agent = self.agents.iter()
                    .find(|a| a.name == agent_name)
                    .ok_or_else(|| AthenaError::Tool(format!("Unknown agent: {}", agent_name)))?;

                eprintln!("📋 Delegating to agent: {}", agent.name);

                let contract = TaskContract {
                    context,
                    goal,
                    constraints: vec![],
                };

                let result = self.executor.run(&contract, agent, &self.llm).await?;

                // Optionally save a lesson
                self.maybe_save_lesson(user_input, &result).await;

                Ok(result)
            }
        }
    }

    async fn classify(&self, user_input: &str, memory_context: &str) -> Result<Classification> {
        let agent_list: String = self.agents.iter()
            .map(|a| format!("- {} — {}", a.name, a.description))
            .collect::<Vec<_>>()
            .join("\n");

        let system = format!(
r#"You are a manager that classifies user requests and delegates tasks.

Available agents:
{}
{}

For each user message, decide:
1. SIMPLE — You can answer directly without tools (greetings, knowledge questions, explanations)
2. COMPLEX — Needs an agent to execute (file operations, shell commands, code tasks)

Respond with JSON:
- Simple: {{"type": "simple", "answer": "your direct answer"}}
- Complex: {{"type": "complex", "agent": "<agent_name>", "goal": "<clear goal for agent>", "context": "<relevant context>"}}"#,
            agent_list,
            memory_context,
        );

        let messages = vec![
            Message::system(&system),
            Message::user(user_input),
        ];

        let response = self.llm.chat(&messages).await?;

        // Parse classification
        if let Some(json) = llm::extract_json(&response) {
            let task_type = json["type"].as_str().unwrap_or("simple");
            if task_type == "complex" {
                let agent_name = json["agent"].as_str().unwrap_or("coder").to_string();
                let goal = json["goal"].as_str().unwrap_or(user_input).to_string();
                let context = json["context"].as_str().unwrap_or("").to_string();
                return Ok(Classification::Complex { agent_name, goal, context });
            }
            if let Some(answer) = json["answer"].as_str() {
                return Ok(Classification::Simple(answer.to_string()));
            }
        }

        // Fallback: treat the raw response as a simple answer
        Ok(Classification::Simple(response))
    }

    async fn maybe_save_lesson(&self, input: &str, result: &str) {
        // Save a brief lesson if the task was interesting enough
        if result.len() > 100 {
            let lesson = format!("Task: {} → Result summary: {}", input, &result[..result.len().min(200)]);
            let _ = self.memory.store("lesson", &lesson);
        }
    }
}

enum Classification {
    Simple(String),
    Complex {
        agent_name: String,
        goal: String,
        context: String,
    },
}
