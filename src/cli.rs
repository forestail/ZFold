use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Small-file solid archive packer for directory snapshots and backups",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Pack(PackArgs),
    List(ListArgs),
    Extract(ExtractArgs),
    Verify(VerifyArgs),
    TrainDict(TrainDictArgs),
}

#[derive(Args, Debug, Clone)]
pub struct PackArgs {
    pub input: PathBuf,

    #[arg(short, long)]
    pub output: PathBuf,

    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub chunk_size: usize,

    #[arg(long, default_value_t = 5)]
    pub level: i32,

    #[arg(long)]
    pub threads: Option<usize>,

    #[arg(long)]
    pub dict: Option<PathBuf>,

    #[arg(long, conflicts_with_all = ["password", "password_env", "password_file"])]
    pub password_prompt: bool,

    #[command(flatten)]
    pub password: PasswordSourceArgs,

    #[arg(long)]
    pub exclude: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    pub archive: PathBuf,

    #[command(flatten)]
    pub password: PasswordSourceArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ExtractArgs {
    pub archive: PathBuf,

    #[arg(short = 'd', long)]
    pub out_dir: PathBuf,

    #[arg(long)]
    pub only: Option<String>,

    #[arg(long)]
    pub prefix: Option<String>,

    #[command(flatten)]
    pub password: PasswordSourceArgs,
}

#[derive(Args, Debug, Clone)]
pub struct VerifyArgs {
    pub archive: PathBuf,

    #[command(flatten)]
    pub password: PasswordSourceArgs,
}

#[derive(Args, Debug, Clone)]
pub struct TrainDictArgs {
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    #[arg(short, long)]
    pub output: PathBuf,

    #[arg(long, default_value_t = 5_000)]
    pub max_samples: usize,

    #[arg(long, default_value_t = 131_072)]
    pub dict_size: usize,

    #[arg(long, default_value_t = 64 * 1024)]
    pub max_sample_bytes: usize,

    #[arg(long)]
    pub include: Vec<String>,

    #[arg(long)]
    pub exclude: Vec<String>,

    #[arg(long, value_delimiter = ',')]
    pub extensions: Vec<String>,

    #[arg(long, value_enum, default_value_t = TrainDictMode::Text)]
    pub mode: TrainDictMode,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum TrainDictMode {
    All,
    Text,
    Html,
}

#[derive(Args, Debug, Clone, Default)]
pub struct PasswordSourceArgs {
    #[arg(long, conflicts_with_all = ["password_env", "password_file"])]
    pub password: Option<String>,

    #[arg(long, conflicts_with_all = ["password", "password_file"])]
    pub password_env: Option<String>,

    #[arg(long, conflicts_with_all = ["password", "password_env"])]
    pub password_file: Option<PathBuf>,
}
