use chrono::TimeZone;
use clap::Clap;
use lido::{
    state::Lido,
    token::{ArithmeticError, Rational},
};
use rusqlite::{params, Connection, Row};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    clock::{Epoch, Slot},
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Keypair,
};
use solido_cli_common::snapshot::{
    Config, OutputMode, SnapshotClient, SnapshotConfig, SnapshotError,
};

#[derive(Clap, Debug)]
pub struct Opts {
    /// URL of cluster to connect to (e.g., https://api.devnet.solana.com for solana devnet)
    // Overwritten by `GeneralOpts` if None.
    #[clap(long, default_value = "http://127.0.0.1:8899")]
    cluster: String,

    /// Whether to output text or json. [default: "text"]
    // Overwritten by `GeneralOpts` if None.
    #[clap(long = "output", possible_values = &["text", "json"])]
    output_mode: Option<OutputMode>,

    /// Unique name for identifying
    #[clap(long, default_value = "solido")]
    pool: String,

    /// Poll frequency in seconds, defaults to 5 minutes.
    #[clap(long, default_value = "300")]
    poll_frequency_seconds: u32,

    /// Location of the SQLite DB file.
    #[clap(long, default_value = "listener.db")]
    db_path: String,
}

struct State {
    pub solido: Lido,
}

impl State {
    pub fn new(
        config: &mut SnapshotConfig,
        solido_program_id: &Pubkey,
        solido_address: &Pubkey,
    ) -> Result<Self, SnapshotError> {
        let solido = config.client.get_solido(solido_address)?;
        Ok(State { solido })
    }
}

#[derive(Debug)]
pub struct ExchangeRate {
    /// Id of the data point.
    id: i32,
    /// Time when the data point was logged.
    timestamp: chrono::DateTime<chrono::Utc>,
    /// Slot when the data point was logged.
    slot: Slot,
    /// Epoch when the data point was logged.
    epoch: Epoch,
    /// Pool identifier, e.g. for Solido would be "solido".
    pool: String,
    /// Price of token A.
    price_lamports_numerator: u64,
    /// Price of token B.
    price_lamports_denominator: u64,
}

pub fn create_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS exchange_rate (
                id          INTEGER PRIMARY KEY,
                --- timestamp is stored in ISO-8601 format.
                timestamp                   TEXT,
                slot                        INTEGER NOT NULL,
                epoch                       INTEGER NOT NULL,
                pool                        TEXT NOT NULL,
                price_lamports_numerator    INTEGER NOT NULL,
                price_lamports_denominator  INTEGER NOT NULL,
                CHECK (price_lamports_denominator>0)
            )",
        [],
    )?;
    Ok(())
}

pub struct IntervalPrices {
    t0: chrono::DateTime<chrono::Utc>,
    t1: chrono::DateTime<chrono::Utc>,
    epoch0: Epoch,
    epoch1: Epoch,
    price0_lamports: Rational,
    price1_lamports: Rational,
}

impl IntervalPrices {
    pub fn duration_wall_time(&self) -> chrono::Duration {
        self.t1 - self.t0
    }
    pub fn duration_epochs(&self) -> u64 {
        self.epoch1 - self.epoch0
    }
    pub fn growth_factor(&self) -> Rational {
        (self.price1_lamports / self.price0_lamports).expect("Division by 0 cannot happen.")
    }
    pub fn annual_growth_factor(&self) -> f64 {
        let year = chrono::Duration::days(365);
        self.growth_factor()
            .to_f64()
            .powf(year.num_seconds() as f64 / self.duration_wall_time().num_seconds() as f64)
    }
    pub fn annual_percentage_rate(&self) -> f64 {
        self.annual_growth_factor().mul_add(100.0, -100.0)
    }
}

pub fn get_apy_for_period(
    tx: rusqlite::Transaction,
    opts: &Opts,
    from_time: chrono::DateTime<chrono::Utc>,
    to_time: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<Option<IntervalPrices>> {
    let row_map = |row: &Row| {
        let timestamp_iso8601: String = row.get(1)?;
        Ok(ExchangeRate {
            id: row.get(0)?,
            timestamp: timestamp_iso8601
                .parse()
                .expect("Invalid timestamp format."),
            slot: row.get(2)?,
            epoch: row.get(3)?,
            pool: row.get(4)?,
            price_lamports_numerator: row.get(5)?,
            price_lamports_denominator: row.get(6)?,
        })
    };

    let (first, last) = {
        // Get first logged minimal logged data based on timestamp that is greater than `from_time`.
        let stmt_first = &mut tx.prepare(
            "WITH prices_epoch AS (
                SELECT *
                FROM exchange_rate
                WHERE epoch = (SELECT MIN(epoch) from exchange_rate where pool = :pool AND timestamp > :t)
              )
              SELECT
                *
              FROM
                prices_epoch
              ORDER BY
                timestamp ASC
            ",
        )?;
        // Get first logged maximal logged data based on timestamp that is smaller than `to_time`.
        let stmt_last =
            &mut tx.prepare("WITH prices_epoch AS (
                SELECT *
                FROM exchange_rate
                WHERE epoch = (SELECT MAX(epoch) from exchange_rate where pool = :pool AND timestamp < :t)
              )
              SELECT
                *
              FROM
                prices_epoch
              ORDER BY
                timestamp ASC
            ")?;
        let mut row_iter =
            stmt_first.query_map([opts.pool.clone(), from_time.to_string()], row_map)?;
        let first = row_iter.next();

        let mut row_iter =
            stmt_last.query_map([opts.pool.clone(), to_time.to_string()], row_map)?;
        let last = row_iter.next();
        (first, last)
    };

    match (first, last) {
        (Some(first), Some(last)) => {
            let first = first?;
            let last = last?;
            // Not enough data, need at least two data points.
            if first.id == last.id {
                Ok(None)
            } else {
                let interval_prices = IntervalPrices {
                    t0: first.timestamp,
                    t1: last.timestamp,
                    epoch0: first.epoch,
                    epoch1: last.epoch,
                    price0_lamports: Rational {
                        numerator: first.price_lamports_numerator,
                        denominator: first.price_lamports_denominator,
                    },
                    price1_lamports: Rational {
                        numerator: last.price_lamports_numerator,
                        denominator: last.price_lamports_denominator,
                    },
                };
                Ok(Some(interval_prices))
            }
        }
        _ => Ok(None),
    }
    // Ok(Some(1.0))
}

pub fn insert_price(conn: &Connection, exchange_rate: ExchangeRate) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO exchange_rate (timestamp, slot, epoch, pool, price_lamports_numerator, price_lamports_denominator) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", 
    params![exchange_rate.timestamp.to_string(), exchange_rate.slot, exchange_rate.epoch, exchange_rate.pool,
        exchange_rate.price_lamports_numerator, exchange_rate.price_lamports_denominator])?;
    Ok(())
}

fn main() {
    let opts = Opts::parse();
    solana_logger::setup_with_default("solana=info");
    let rpc_client = RpcClient::new_with_commitment(opts.cluster, CommitmentConfig::confirmed());
    let snapshot_client = SnapshotClient::new(rpc_client);

    let output_mode = opts.output_mode.unwrap();

    // Our config has a signer, which for this program we will not use, since we
    // only observe information from the Solana blockchain.
    let signer = Keypair::new();
    let config = Config {
        client: snapshot_client,
        signer: &signer,
        output_mode,
    };

    let conn = Connection::open(&opts.db_path).expect("Failed to open sqlite connection.");
    create_db(&conn).expect("Failed to create database.");
}

#[test]
fn test_get_average_apy() {
    let opts = Opts {
        cluster: "http://127.0.0.1:8899".to_owned(),
        output_mode: None,
        pool: "solido".to_owned(),
        poll_frequency_seconds: 1,
        db_path: "listener".to_owned(),
    };
    let mut conn = Connection::open_in_memory().expect("Failed to open sqlite connection.");
    create_db(&conn).unwrap();
    let exchange_rate = ExchangeRate {
        id: 0,
        timestamp: chrono::Utc.ymd(2020, 8, 8).and_hms(0, 0, 0),
        slot: 1,
        epoch: 1,
        pool: opts.pool.clone(),
        price_lamports_numerator: 1,
        price_lamports_denominator: 1,
    };
    insert_price(&conn, exchange_rate).unwrap();
    let exchange_rate = ExchangeRate {
        id: 0,
        timestamp: chrono::Utc.ymd(2021, 1, 8).and_hms(0, 0, 0),
        slot: 2,
        epoch: 2,
        pool: opts.pool.clone(),
        price_lamports_numerator: 1394458971361025,
        price_lamports_denominator: 1367327673971744,
    };
    insert_price(&conn, exchange_rate).unwrap();
    let apy = get_apy_for_period(
        conn.transaction().unwrap(),
        &opts,
        chrono::Utc.ymd(2020, 7, 7).and_hms(0, 0, 0),
        chrono::Utc.ymd(2021, 7, 8).and_hms(0, 0, 0),
    )
    .expect("Failed when getting APY for period");
    assert_eq!(apy.unwrap().annual_percentage_rate(), 4.7989255185326485);
}
