//! Classic Eliza pattern matching implementation.
//!
//! This module implements the pattern-matching conversational system
//! originally created by Joseph Weizenbaum in 1966.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use regex::Regex;
use std::collections::HashMap;

/// A pattern-response pair for Eliza's conversation system.
#[derive(Debug, Clone)]
struct Pattern {
    /// Regular expression pattern to match user input
    pattern: Regex,
    /// Possible responses (one chosen at random)
    responses: Vec<String>,
    /// Priority for this pattern (higher = checked first)
    priority: usize,
}

/// The Eliza chatbot engine.
pub struct Eliza {
    patterns: Vec<Pattern>,
    reflections: HashMap<String, String>,
    rng: StdRng,
}

impl Eliza {
    /// Create a new Eliza instance with classic patterns.
    /// Uses a fixed seed for deterministic testing.
    pub fn new() -> Self {
        Self::with_seed(42)
    }

    /// Create a new Eliza instance with a specific seed.
    pub fn with_seed(seed: u64) -> Self {
        let mut eliza = Self {
            patterns: Vec::new(),
            reflections: Self::build_reflections(),
            rng: StdRng::seed_from_u64(seed),
        };
        eliza.load_patterns();
        eliza
    }

    /// Build the word reflection map for pronoun swapping.
    fn build_reflections() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("i".to_string(), "you".to_string());
        map.insert("me".to_string(), "you".to_string());
        map.insert("my".to_string(), "your".to_string());
        map.insert("mine".to_string(), "yours".to_string());
        map.insert("am".to_string(), "are".to_string());
        map.insert("i'm".to_string(), "you're".to_string());
        map.insert("you".to_string(), "I".to_string());
        map.insert("your".to_string(), "my".to_string());
        map.insert("yours".to_string(), "mine".to_string());
        map.insert("are".to_string(), "am".to_string());
        map.insert("you're".to_string(), "I'm".to_string());
        map
    }

    /// Load the classic Eliza pattern database.
    fn load_patterns(&mut self) {
        // High priority patterns for specific psychological keywords
        self.add_pattern(
            r"(?i).*\b(father|dad|mother|mom|parent|family)\b.*",
            vec![
                "Tell me more about your family.",
                "How do you feel about your family?",
                "Your family seems to be important to you.",
            ],
            10,
        );

        self.add_pattern(
            r"(?i).*\b(dream|nightmare)\b.*",
            vec![
                "What does that dream suggest to you?",
                "Do you dream often?",
                "What persons appear in your dreams?",
                "Do you believe that dreams have something to do with your problem?",
            ],
            9,
        );

        self.add_pattern(
            r"(?i).*\bsorry\b.*",
            vec![
                "Please don't apologize.",
                "Apologies are not necessary.",
                "What feelings do you have when you apologize?",
            ],
            8,
        );

        // Medium priority patterns with capture groups
        self.add_pattern(
            r"(?i).*\bi am ([\w\s]+)",
            vec![
                "How long have you been {}?",
                "Do you believe it is normal to be {}?",
                "Do you enjoy being {}?",
            ],
            7,
        );

        self.add_pattern(
            r"(?i).*\bi feel ([\w\s]+)",
            vec![
                "Do you often feel {}?",
                "What makes you feel {}?",
                "When do you usually feel {}?",
            ],
            7,
        );

        self.add_pattern(
            r"(?i).*\bi (want|need) ([\w\s]+)",
            vec![
                "Why do you {} {}?",
                "Would it really help you to {} {}?",
                "Are you sure you {} {}?",
            ],
            7,
        );

        self.add_pattern(
            r"(?i).*\bwhy don'?t you ([\w\s]+)",
            vec![
                "Do you really think I don't {}?",
                "Perhaps eventually I will {}.",
                "Do you really want me to {}?",
            ],
            6,
        );

        self.add_pattern(
            r"(?i).*\bwhy can'?t i ([\w\s]+)",
            vec![
                "Do you think you should be able to {}?",
                "If you could {}, what would you do?",
                "I don't know -- why can't you {}?",
            ],
            6,
        );

        self.add_pattern(
            r"(?i).*\byes\b.*",
            vec!["You seem quite sure.", "I see.", "I understand."],
            3,
        );

        self.add_pattern(
            r"(?i).*\bno\b.*",
            vec![
                "Why not?",
                "You are being a bit negative.",
                "Are you saying 'no' just to be negative?",
            ],
            3,
        );

        // Low priority catch-all patterns
        self.add_pattern(
            r"(?i).*(hello|hi|hey).*",
            vec![
                "Hello. How are you feeling today?",
                "Hi there. What brings you here today?",
            ],
            2,
        );

        self.add_pattern(
            r"(?i).*(bye|goodbye|exit|quit).*",
            vec![
                "Goodbye. Thank you for talking to me.",
                "Goodbye. This was really a nice talk.",
            ],
            2,
        );

        // Default fallback patterns (lowest priority)
        self.add_pattern(
            r"(?i).*\?",
            vec![
                "Why do you ask that?",
                "Please consider whether you can answer your own question.",
                "Perhaps the answer lies within yourself?",
            ],
            1,
        );

        self.add_pattern(
            r".*",
            vec![
                "Please tell me more.",
                "I see. Go on.",
                "That's interesting. Can you elaborate?",
                "How does that make you feel?",
                "Why do you say that?",
            ],
            0,
        );
    }

    /// Add a pattern to the database.
    fn add_pattern(&mut self, pattern: &str, responses: Vec<&str>, priority: usize) {
        let regex = Regex::new(pattern).expect("Invalid regex pattern");
        self.patterns.push(Pattern {
            pattern: regex,
            responses: responses.into_iter().map(|s| s.to_string()).collect(),
            priority,
        });
    }

    /// Reflect pronouns in the captured text.
    fn reflect(&self, text: &str) -> String {
        let words: Vec<&str> = text.split_whitespace().collect();
        let reflected: Vec<String> = words
            .iter()
            .map(|word| {
                let lower = word.to_lowercase();
                self.reflections
                    .get(&lower)
                    .map(|r| r.clone())
                    .unwrap_or_else(|| word.to_string())
            })
            .collect();
        reflected.join(" ")
    }

    /// Generate a response to user input.
    pub fn respond(&mut self, input: &str) -> String {
        let input = input.trim();

        // Sort patterns by priority (highest first)
        let mut sorted_patterns = self.patterns.clone();
        sorted_patterns.sort_by(|a, b| b.priority.cmp(&a.priority));

        // Find first matching pattern
        for pattern in &sorted_patterns {
            if let Some(captures) = pattern.pattern.captures(input) {
                // Choose a response using the seeded RNG
                let response_index = self.rng.random_range(0..pattern.responses.len());
                let response_template = &pattern.responses[response_index];

                // Fill in captures with reflection
                let mut response = response_template.clone();
                for i in 1..captures.len() {
                    if let Some(capture) = captures.get(i) {
                        let reflected = self.reflect(capture.as_str());
                        response = response.replacen("{}", &reflected, 1);
                    }
                }

                return response;
            }
        }

        // This should never happen due to catch-all pattern
        "I'm not sure I understand.".to_string()
    }
}

impl Default for Eliza {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    #[test]
    fn test_basic_responses() {
        let mut eliza = Eliza::new();

        let response = eliza.respond("Hello");
        expect!["Hello. How are you feeling today?"].assert_eq(&response);

        let response = eliza.respond("I am sad");
        expect!["Do you believe it is normal to be sad?"].assert_eq(&response);
    }

    #[test]
    fn test_reflection() {
        let eliza = Eliza::new();

        let reflected = eliza.reflect("I am happy");
        expect![[r#"you are happy"#]].assert_eq(&reflected);

        let reflected = eliza.reflect("my mother");
        expect![[r#"your mother"#]].assert_eq(&reflected);
    }

    #[test]
    fn test_pattern_priority() {
        let mut eliza = Eliza::new();

        // "sorry" should match high priority pattern
        let response = eliza.respond("I am sorry");
        expect!["Please don't apologize."].assert_eq(&response);
    }

    #[test]
    fn test_punctuation_handling() {
        let mut eliza = Eliza::new();

        // Trailing punctuation should be excluded from captures
        let response = eliza.respond("I feel sad.");
        expect!["Do you often feel sad?"].assert_eq(&response);
    }

    #[test]
    fn test_deterministic_responses() {
        // Two Eliza instances with the same seed should produce identical responses
        let mut eliza1 = Eliza::with_seed(42);
        let mut eliza2 = Eliza::with_seed(42);

        let inputs = vec![
            "Hello",
            "I am sad",
            "I feel worried",
            "Why don't you help me",
            "I need assistance",
        ];

        for input in inputs {
            let response1 = eliza1.respond(input);
            let response2 = eliza2.respond(input);
            assert_eq!(
                response1, response2,
                "Responses should be identical for input: {}",
                input
            );
        }

        // Different seeds should (likely) produce different responses
        let mut eliza3 = Eliza::with_seed(123);
        let response_different = eliza3.respond("Hello");

        // Reset eliza1 to test again with same seed
        let mut eliza1 = Eliza::with_seed(42);
        let response_same = eliza1.respond("Hello");

        // Note: This might occasionally fail if both seeds happen to pick the same response
        // but it's very unlikely given the pattern database size
        assert_ne!(
            response_same, response_different,
            "Different seeds should likely produce different responses"
        );
    }
}
