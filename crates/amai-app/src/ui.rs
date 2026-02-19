use soul_terminal_core::*;
use soul_terminal_widgets::*;

/// A chat message for display.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
}

/// Render a list of chat messages as widgets.
pub fn render_messages(messages: &[ChatMessage]) -> Container {
    let mut container = Container::new("messages")
        .with_gap(8.0)
        .with_block_style(BlockStyle::default().with_padding(Spacing::symmetric(8.0, 16.0)));

    for (i, msg) in messages.iter().enumerate() {
        let (role_label, role_color) = match msg.role {
            ChatRole::User => ("you", Color::from_hex("#7aa2f7")),
            ChatRole::Assistant => ("amai", Color::from_hex("#9ece6a")),
            ChatRole::Tool => {
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                (name, Color::from_hex("#ff9e64"))
            }
            ChatRole::System => ("system", Color::from_hex("#565f89")),
        };

        let msg_container = Container::new(format!("msg_{i}"))
            .with_block_style(
                BlockStyle::default()
                    .with_padding(Spacing::all(8.0))
                    .with_corner_radius(4.0),
            )
            .with_gap(4.0)
            .add_child(Box::new(
                TextWidget::new(format!("role_{i}"), format!("[{role_label}]"))
                    .with_color(role_color)
                    .with_bold(true)
                    .with_font_size(12.0),
            ))
            .add_child(Box::new(TextWidget::new(
                format!("content_{i}"),
                msg.content.clone(),
            )));

        container = container.add_child(Box::new(msg_container));
    }

    container
}

/// Render the status bar at the bottom.
pub fn render_status_bar(
    model: &str,
    tokens_used: usize,
    cost_usd: f64,
    context_pct: f32,
) -> Container {
    let context_color = if context_pct < 0.5 {
        Color::from_hex("#9ece6a") // green
    } else if context_pct < 0.7 {
        Color::from_hex("#e0af68") // yellow
    } else if context_pct < 0.9 {
        Color::from_hex("#ff9e64") // orange
    } else {
        Color::from_hex("#f7768e") // red
    };

    Container::new("status_bar")
        .with_direction(Direction::Row)
        .with_gap(16.0)
        .with_block_style(
            BlockStyle::default()
                .with_padding(Spacing::symmetric(4.0, 16.0))
                .with_background(Color::from_hex("#1a1b26")),
        )
        .add_child(Box::new(
            TextWidget::new("model_label", model.to_string())
                .with_font_size(12.0)
                .with_color(Color::from_hex("#7aa2f7")),
        ))
        .add_child(Box::new(
            TextWidget::new(
                "tokens_label",
                format!("{} tokens", format_number(tokens_used)),
            )
            .with_font_size(12.0)
            .with_color(Color::from_hex("#565f89")),
        ))
        .add_child(Box::new(
            TextWidget::new("cost_label", format!("${:.4}", cost_usd))
                .with_font_size(12.0)
                .with_color(Color::from_hex("#565f89")),
        ))
        .add_child(Box::new(
            TextWidget::new(
                "context_label",
                format!("ctx: {:.0}%", context_pct * 100.0),
            )
            .with_font_size(12.0)
            .with_color(context_color),
        ))
}

/// Render the input bar at the bottom.
pub fn render_input_bar(_input: &InputWidget) -> Container {
    Container::new("input_bar")
        .with_direction(Direction::Row)
        .with_gap(8.0)
        .with_block_style(
            BlockStyle::default().with_padding(Spacing::symmetric(8.0, 16.0)),
        )
        .add_child(Box::new(
            TextWidget::new("prompt_char", ">")
                .with_color(Color::from_hex("#7aa2f7"))
                .with_bold(true),
        ))
}

fn format_number(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
