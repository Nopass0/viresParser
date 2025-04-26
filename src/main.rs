use anyhow::{anyhow, Result};
use chrono::{NaiveDateTime, TimeZone, Utc};
use dotenvy::dotenv;
use futures::future::join_all;
use reqwest::{header::USER_AGENT, Client, ClientBuilder};
use reqwest_cookie_store::CookieStoreMutex;
use scraper::{Html, Selector};
use serde::Deserialize;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    PgPool, Postgres, Transaction,
};
use std::{collections::HashMap, env, sync::Arc, time::Duration};
use tokio::{task, time::sleep};
use tracing::{error, info, instrument};

const BASE: &str = "https://trade.vires.pro";
const INTERVAL_SECS: u64 = 30;

// ─────────────────────────────── модели ────────────────────────────────

#[derive(Debug)]
struct Payin {
    created_at: NaiveDateTime,
    sum_rub: f64,
    sum_usdt: f64,
    card: String,
    fio: String,
    bank: String,
    uuid: String,
}

#[derive(Debug, Deserialize, sqlx::FromRow)]
struct Cabinet {
    id: i32,
    login: String,
    password: String,
}

// ─────────────────────────────── main ──────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt().with_env_filter("info").init();

    let db_url =
        env::var("DATABASE_URL").map_err(|_| anyhow!("DATABASE_URL не задан в .env"))?;
    let pool = connect_with_retries(&db_url, 3).await?;

    loop {
        if let Err(e) = run_cycle(&pool).await {
            error!("цикл завершился ошибкой: {e:?}");
        }
        sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

// ────────────────────── обход всех кабинетов ───────────────────────────

async fn run_cycle(pool: &PgPool) -> Result<()> {
    let cabs: Vec<Cabinet> =
        sqlx::query_as::<_, Cabinet>(r#"SELECT id, login, password FROM "ViresCabinet""#)
            .fetch_all(pool)
            .await?;

    info!("найдено кабинетов: {}", cabs.len());

    let tasks = cabs.into_iter().map(|cab| {
        let pool = pool.clone();
        task::spawn(async move {
            if let Err(e) = handle_cabinet(&pool, &cab).await {
                error!("cabinet {}: {e:?}", cab.id);
            }
        })
    });

    join_all(tasks).await;
    Ok(())
}

// ────────────────────── обработка одного кабинета ──────────────────────

#[instrument(skip_all, fields(cabinet = cab.id))]
async fn handle_cabinet(pool: &PgPool, cab: &Cabinet) -> Result<()> {
    let ops = fetch_payins(&cab.login, &cab.password).await?;
    if ops.is_empty() {
        return Ok(());
    }
    insert_operations(pool, cab.id, &ops).await?;
    info!("cabinet {}: добавлено {}", cab.id, ops.len());
    Ok(())
}

// ──────────────────────────── Postgres ─────────────────────────────────

async fn connect_with_retries(url: &str, attempts: u8) -> Result<PgPool> {
    let options = url
        .parse::<PgConnectOptions>()?
        .statement_cache_capacity(0); // без client-side кеша prepared-stmt’ов

    for n in 1..=attempts {
        match PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options.clone())
            .await
        {
            Ok(p) => return Ok(p),
            Err(e) => {
                error!("подключение к БД не удалось (попытка {n}/{attempts}): {e}");
                sleep(Duration::from_secs(2_u64.pow(n.into()))).await;
            }
        }
    }
    Err(anyhow!("не удалось подключиться к базе"))
}

async fn insert_operations(pool: &PgPool, cab_id: i32, ops: &[Payin]) -> Result<()> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

    for op in ops {
        sqlx::query(
            r#"
            INSERT INTO "ViresTransactionPayin"
                   ("cabinetId","createdAt", sum_rub, sum_usdt, card, fio, bank, uuid)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
            ON CONFLICT (uuid) DO NOTHING
            "#,
        )
        .bind(cab_id)
        .bind(Utc.from_utc_datetime(&op.created_at))
        .bind(op.sum_rub)
        .bind(op.sum_usdt)
        .bind(&op.card)
        .bind(&op.fio)
        .bind(&op.bank)
        .bind(&op.uuid)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

// ───────────────────────────── scraping ────────────────────────────────

async fn fetch_payins(login: &str, password: &str) -> Result<Vec<Payin>> {
    let jar = CookieStoreMutex::default();
    let client: Client = ClientBuilder::new()
        .cookie_provider(Arc::new(jar))
        .user_agent("Mozilla/5.0 (vires-worker/0.1)")
        .build()?;

    // 1) CSRF
    let html = client
        .get(format!("{BASE}/site/login"))
        .send()
        .await?
        .text()
        .await?;
    let csrf = extract_csrf(&html)?;

    // 2) логин
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("_csrf-frontend", &csrf);
    form.insert("LoginForm[email]", login);
    form.insert("LoginForm[password]", password);

    client
        .post(format!("{BASE}/site/login"))
        .form(&form)
        .header(USER_AGENT, "Mozilla/5.0")
        .send()
        .await?
        .error_for_status()?; // будет 302 на /cabinet

    // 3) сбор «Готов»
    let mut page = 1_u32;
    let mut last = 1;
    let mut out = Vec::<Payin>::new();

    loop {
        let html = client
            .get(format!("{BASE}/cabinet/payin?page={page}"))
            .send()
            .await?
            .text()
            .await?;

        if page == 1 {
            last = extract_last_page(&html)?;
        }
        out.extend(parse_table(&html)?);

        if page >= last {
            break;
        }
        page += 1;
    }
    Ok(out)
}

fn extract_csrf(html: &str) -> Result<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"input[name="_csrf-frontend"]"#).unwrap();
    Ok(doc
        .select(&sel)
        .next()
        .and_then(|n| n.value().attr("value"))
        .ok_or_else(|| anyhow!("CSRF-токен не найден"))?
        .to_owned())
}

fn extract_last_page(html: &str) -> Result<u32> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("ul.pagination li a").unwrap();
    Ok(doc
        .select(&sel)
        .filter_map(|n| n.text().next()?.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(1))
}

fn parse_table(html: &str) -> Result<Vec<Payin>> {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse(r#"table.list_operations tr[data-id]"#).unwrap();
    let cell_sel = Selector::parse("td").unwrap();

    let mut out = Vec::new();

    for row in doc.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| c.text().collect::<String>().trim().to_string())
            .collect();
        if cells.len() < 9 || cells[1] != "Готов" {
            continue;
        }

        let created_at =
            NaiveDateTime::parse_from_str(&cells[2], "%d.%m.%Y %H:%M")?;
        let sum_rub = parse_amount(&cells[3])?;
        let sum_usdt = parse_amount(&cells[4])?;

        out.push(Payin {
            created_at,
            sum_rub,
            sum_usdt,
            card: cells[5].clone(),
            fio: cells[6].clone(),
            bank: cells[7].clone(),
            uuid: cells[0].clone(),
        });
    }
    Ok(out)
}

fn parse_amount(s: &str) -> Result<f64> {
    let num = s
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("не удалось прочитать сумму"))?;
    Ok(num.replace(',', ".").parse()?)
}
