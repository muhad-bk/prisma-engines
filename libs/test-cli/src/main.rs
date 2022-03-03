#![allow(clippy::print_literal)] // it is just wrong in this case

mod diagnose_migration_history;

use anyhow::Context;
use colored::Colorize;
use introspection_connector::CompositeTypeDepth;
use migration_connector::BoxFuture;
use migration_core::json_rpc::types::*;
use std::{fmt, fs::File, io::Read, str::FromStr, sync::Arc};
use structopt::*;

#[derive(Debug, StructOpt)]
#[structopt(version = env!("GIT_HASH"))]
enum Command {
    /// Introspect a database
    Introspect {
        /// URL of the database to introspect.
        #[structopt(long)]
        url: Option<String>,
        /// Path to the schema file to introspect for.
        #[structopt(long = "file-path")]
        file_path: Option<String>,
        /// How many layers of composite types we introspect before switching to Json.
        #[structopt(long)]
        composite_type_depth: Option<isize>,
    },
    /// Generate DMMF from a schema, or directly from a database URL.
    Dmmf(DmmfCommand),
    /// Push a prisma schema directly to the database.
    SchemaPush(SchemaPush),
    /// DiagnoseMigrationHistory wrapper
    DiagnoseMigrationHistory(DiagnoseMigrationHistory),
    /// Counterpart to the CLI migrate diff.
    MigrateDiff(MigrateDiff),
    /// Validate the given data model.
    ValidateDatamodel(ValidateDatamodel),
    /// Clear the data and DDL of the given database.
    ResetDatabase(ResetDatabase),
    /// Clear the data and DDL of the given database.
    CreateDatabase(CreateDatabase),
    /// Create a new migration to the given directory.
    CreateMigration(CreateMigration),
    /// Apply all unapplied migrations from the given directory.
    ApplyMigrations(ApplyMigrations),
}

#[derive(Debug, StructOpt)]
struct DmmfCommand {
    /// The path to the `query-engine` binary. Defaults to the value of the `PRISMA_BINARY_PATH`
    /// env var, or just `query-engine`.
    #[structopt(env = "PRISMA_BINARY_PATH", default_value = "query-engine")]
    query_engine_binary_path: String,
    /// A database URL to introspect and generate DMMF for.
    #[structopt(long = "url")]
    url: Option<String>,
    /// Path of the prisma schema to generate DMMF for.
    #[structopt(long = "file-path")]
    file_path: Option<String>,
}

#[derive(Debug, StructOpt)]
struct SchemaPush {
    schema_path: String,
    #[structopt(long)]
    force: bool,
}

#[derive(StructOpt, Debug)]
struct DiagnoseMigrationHistory {
    schema_path: String,
    migrations_directory_path: String,
}

#[derive(Debug, Clone, Copy)]
enum DiffOutputType {
    Summary,
    Ddl,
}

impl Default for DiffOutputType {
    fn default() -> Self {
        Self::Summary
    }
}

impl FromStr for DiffOutputType {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "summary" => Ok(Self::Summary),
            "ddl" => Ok(Self::Ddl),
            _ => {
                let kind = std::io::ErrorKind::InvalidInput;
                Err(std::io::Error::new(kind, format!("Invalid output type: `{}`", s)))
            }
        }
    }
}

impl fmt::Display for DiffOutputType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffOutputType::Summary => write!(f, "summary"),
            DiffOutputType::Ddl => write!(f, "ddl"),
        }
    }
}

#[derive(StructOpt, Debug)]
#[allow(dead_code)]
struct MigrateDiff {
    #[structopt(long = "from-schema-datamodel")]
    from_schema_datamodel: Option<String>,
    #[structopt(long = "to-schema-datamodel")]
    to_schema_datamodel: Option<String>,

    #[structopt(long = "from-schema-datasource")]
    from_schema_datasource: Option<String>,
    #[structopt(long = "to-schema-datasource")]
    to_schema_datasource: Option<String>,

    #[structopt(long = "from-url")]
    from_url: Option<String>,
    #[structopt(long = "to-url")]
    to_url: Option<String>,

    #[structopt(long = "from-empty")]
    from_empty: bool,
    #[structopt(long = "to-empty")]
    to_empty: bool,

    /// Output SQL (default: false). Otherwise will produce a summary.
    #[structopt(long)]
    script: bool,
}

#[derive(StructOpt, Debug)]
struct ValidateDatamodel {
    /// Path to the prisma data model.
    schema_path: String,
}

#[derive(StructOpt, Debug)]
struct ResetDatabase {
    /// Path to the prisma data model.
    schema_path: String,
}

#[derive(StructOpt, Debug)]
struct CreateDatabase {
    /// Path to the prisma data model.
    schema_path: String,
}

#[derive(StructOpt, Debug)]
struct CreateMigration {
    /// The filesystem path of the migrations directory to use
    migrations_path: String,
    /// The current prisma schema to use as a target for the generated migration
    schema_path: String,
    /// The user-given name for the migration.
    name: String,
}

#[derive(StructOpt, Debug)]
struct ApplyMigrations {
    /// The location of the migrations directory.
    migrations_directory_path: String,
    /// The current prisma schema to use as a target for the generated migration
    schema_path: String,
}

impl From<ApplyMigrations> for ApplyMigrationsInput {
    fn from(am: ApplyMigrations) -> Self {
        Self {
            migrations_directory_path: am.migrations_directory_path,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logger();

    match Command::from_args() {
        Command::DiagnoseMigrationHistory(cmd) => cmd.execute().await?,
        Command::Dmmf(cmd) => generate_dmmf(&cmd).await?,
        Command::SchemaPush(cmd) => schema_push(&cmd).await?,
        Command::MigrateDiff(cmd) => migrate_diff(&cmd).await?,
        Command::Introspect {
            url,
            file_path,
            composite_type_depth,
        } => {
            if url.as_ref().xor(file_path.as_ref()).is_none() {
                anyhow::bail!(
                    "{}",
                    "Exactly one of --url or --file-path must be provided".bold().red()
                );
            }

            let schema = if let Some(file_path) = file_path {
                read_datamodel_from_file(&file_path)?
            } else if let Some(url) = url {
                minimal_schema_from_url(&url)?
            } else {
                unreachable!()
            };

            //todo configurable
            let introspected = introspection_core::RpcImpl::introspect_internal(
                schema,
                false,
                CompositeTypeDepth::from(composite_type_depth.unwrap_or(0)),
            )
            .await
            .map_err(|err| anyhow::anyhow!("{:?}", err.data))?;

            println!("{}", introspected);
        }
        Command::ValidateDatamodel(cmd) => {
            use std::io::Read as _;

            let mut file = std::fs::File::open(&cmd.schema_path).expect("error opening datamodel file");

            let mut datamodel = String::new();
            file.read_to_string(&mut datamodel).unwrap();

            if let Err(e) = datamodel::parse_datamodel(&datamodel) {
                let pretty = e.to_pretty_string("schema.prisma", &datamodel);
                println!("{pretty}");
            };
        }
        Command::ResetDatabase(cmd) => {
            let schema = read_datamodel_from_file(&cmd.schema_path).context("Error reading the schema from file")?;
            let api = migration_core::migration_api(Some(schema), None)?;

            api.reset().await?;
        }
        Command::CreateDatabase(cmd) => {
            let schema = read_datamodel_from_file(&cmd.schema_path).context("Error reading the schema from file")?;
            let api = migration_core::migration_api(Some(schema.clone()), None)?;

            api.create_database(CreateDatabaseParams {
                datasource: DatasourceParam::SchemaString(SchemaContainer { schema }),
            })
            .await?;
        }
        Command::CreateMigration(cmd) => {
            let prisma_schema =
                read_datamodel_from_file(&cmd.schema_path).context("Error reading the schema from file")?;

            let api = migration_core::migration_api(Some(prisma_schema.clone()), None)?;

            let input = CreateMigrationInput {
                migrations_directory_path: cmd.migrations_path,
                prisma_schema,
                migration_name: cmd.name,
                draft: true,
            };

            api.create_migration(input).await?;
        }
        Command::ApplyMigrations(cmd) => {
            let prisma_schema =
                read_datamodel_from_file(&cmd.schema_path).context("Error reading the schema from file")?;

            let api = migration_core::migration_api(Some(prisma_schema), None)?;
            api.apply_migrations(cmd.into()).await?;
        }
    }

    Ok(())
}

fn read_datamodel_from_file(path: &str) -> std::io::Result<String> {
    use std::path::Path;

    eprintln!("{} {}", "reading the prisma schema from".bold(), path.yellow());

    let path = Path::new(path);
    let mut file = File::open(path)?;

    let mut out = String::new();
    file.read_to_string(&mut out)?;

    Ok(out)
}

fn minimal_schema_from_url(url: &str) -> anyhow::Result<String> {
    let provider = match url.split("://").next() {
        Some("file") | Some("sqlite") => "sqlite",
        Some(s) if s.starts_with("postgres") => "postgresql",
        Some("mysql") => "mysql",
        Some("sqlserver") => "sqlserver",
        Some("mongodb" | "mongodb+srv") => "mongodb",
        _ => anyhow::bail!("Could not extract a provider from the URL"),
    };

    let schema = format!(
        r#"
            datasource db {{
              provider = "{}"
              url = "{}"
            }}

            generator js {{
              provider        = "prisma-client-js"
              previewFeatures = ["mongodb"]
            }}
        "#,
        provider, url
    );

    Ok(schema)
}

async fn generate_dmmf(cmd: &DmmfCommand) -> anyhow::Result<()> {
    let schema_path: String = {
        if let Some(url) = cmd.url.as_ref() {
            let skeleton = minimal_schema_from_url(url)?;
            //todo make this configurable
            let introspected =
                introspection_core::RpcImpl::introspect_internal(skeleton, false, CompositeTypeDepth::Infinite)
                    .await
                    .map_err(|err| anyhow::anyhow!("{:?}", err.data))?;

            eprintln!("{}", "Schema was successfully introspected from database URL".green());

            let path = "/tmp/prisma-test-cli-introspected.prisma";
            std::fs::write(path, introspected.datamodel)?;
            path.to_owned()
        } else if let Some(file_path) = cmd.file_path.as_ref() {
            file_path.clone()
        } else {
            eprintln!(
                "{} {} {} {}",
                "Please provide one of".yellow(),
                "--url".bold(),
                "or".yellow(),
                "--file-path".bold()
            );
            std::process::exit(1)
        }
    };

    eprintln!(
        "{} {}",
        "Using the query engine binary at".yellow(),
        cmd.query_engine_binary_path.bold()
    );

    let cmd = std::process::Command::new(&cmd.query_engine_binary_path)
        .arg("cli")
        .arg("dmmf")
        .env("PRISMA_DML_PATH", schema_path)
        .spawn()?;

    cmd.wait_with_output()?;

    Ok(())
}

async fn schema_push(cmd: &SchemaPush) -> anyhow::Result<()> {
    let schema = read_datamodel_from_file(&cmd.schema_path).context("Error reading the schema from file")?;
    let api = migration_core::migration_api(Some(schema.clone()), None)?;

    let response = api
        .schema_push(SchemaPushInput {
            schema,
            force: cmd.force,
        })
        .await?;

    if !response.warnings.is_empty() {
        eprintln!("⚠️  {}", "Warnings".bright_yellow().bold());

        for warning in &response.warnings {
            eprintln!("- {}", warning.bright_yellow())
        }
    }

    if !response.unexecutable.is_empty() {
        eprintln!("☢️  {}", "Unexecutable steps".bright_red().bold());

        for unexecutable in &response.unexecutable {
            eprintln!("- {}", unexecutable.bright_red())
        }
    }

    if response.executed_steps > 0 {
        eprintln!(
            "{}  {}",
            "✔️".bold(),
            format!("Schema pushed to database. ({} steps)", response.executed_steps).green()
        );
    } else if response.unexecutable.is_empty() && response.warnings.is_empty() && response.executed_steps == 0 {
        eprintln!(
            "{}  {}",
            "✔️".bold(),
            "No changes to push. Prisma schema and database are in sync.".green()
        );
    } else {
        eprintln!(
            "{}  {}",
            "❌".bold(),
            "The schema was not pushed. Pass the --force flag to ignore warnings."
        );
        std::process::exit(1);
    }

    Ok(())
}

struct DiffHost;

impl migration_connector::ConnectorHost for DiffHost {
    fn print(&self, s: &str) -> BoxFuture<'_, migration_core::CoreResult<()>> {
        print!("{}", s);
        Box::pin(std::future::ready(Ok(())))
    }
}

async fn migrate_diff(cmd: &MigrateDiff) -> anyhow::Result<()> {
    use migration_core::json_rpc::types::*;

    let api = migration_core::migration_api(None, Some(Arc::new(DiffHost)))?;
    let to = if let Some(to_schema_datamodel) = &cmd.to_schema_datamodel {
        DiffTarget::SchemaDatamodel(SchemaContainer {
            schema: to_schema_datamodel.clone(),
        })
    } else {
        todo!("can't handle {:?} yet", cmd)
    };
    let from = if let Some(url) = &cmd.from_url {
        DiffTarget::Url(UrlContainer { url: url.clone() })
    } else {
        todo!("can't handle {:?} yet", cmd)
    };

    let input = DiffParams {
        exit_code: None,
        from,
        script: cmd.script,
        shadow_database_url: None, // TODO
        to,
    };

    api.diff(input).await?;

    Ok(())
}

fn init_logger() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;

    use tracing_subscriber::{EnvFilter, FmtSubscriber};

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .finish()
        .with(ErrorLayer::default());

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|err| eprintln!("Error initializing the global logger: {}", err))
        .ok();
}
