#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SourceKind {
    Control,
    Poll,
}

pub async fn spike_control(_socket: std::path::PathBuf) -> anyhow::Result<()> {
    anyhow::bail!("not yet")
}
