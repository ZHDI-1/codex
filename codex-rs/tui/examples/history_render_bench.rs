fn main() -> anyhow::Result<()> {
    println!("{}", codex_tui::history_render_benchmark::run()?);
    Ok(())
}
