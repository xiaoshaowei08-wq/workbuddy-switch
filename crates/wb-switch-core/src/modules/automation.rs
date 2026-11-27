//! 定时任务的跨账号复制 / 转移。
//!
//! 定时任务不是会话：它们存在客户端数据根的 `workbuddy.db` **automations 表**，每行
//! 带 `owner_user_id` / `owner_status` 两个归属字段。客户端有三处都过同一道门：
//!
//!   1. 列表快照 `filterAutomationsForCurrentUser`
//!   2. 到点派发 `startAutomationRun`（不可见直接放弃，连排队都不进）
//!   3. 手动测试 `resolveAutomationOwnerForAction`（拒绝理由 `owned_by_other`）
//!
//! 判定口径（客户端 `isAutomationVisibleToUser`）：`owner_status` 为 `confirmed` /
//! `inferred` 时，必须 `owner_user_id === 当前登录 uid`；其它状态（`legacy_unassigned`
//! / `conflict`）对所有账号可见。切号只重写 auth 文件、不动这张表，所以旧任务的 owner
//! 仍是旧 uid —— 列表里消失、到点也不执行，这就是「切号后定时任务失效」的全部原因。
//!
//! 本模块把勾选的任务落到目标账号：
//!   - **复制**：插新行（新 id、新归属），源账号那一份原样保留；
//!   - **转移**：只改归属字段，任务跟着账号走，源账号失去。
//!
//! 幂等不只靠 links 里的 `sourceId`：往返切换会让同一任务每次换一个新 id，只认 id
//! 就会一路复制出多份。落库前还要按「任务定义指纹」比对目标账号已有任务——只比要做什么、
//! 何时做、用什么配置做，不比 id / 归属 / 运行时间 / 状态 / 空间；命中就跳过，不新增副本。
//!
//! 写入一律带 `owner_status='confirmed'`：客户端启动时的 `reconcileAutomationOwners`
//! 只校正非 `confirmed` 的行，写别的状态会被它按历史会话「纠正」回去。
//!
//! 三个已知边界（都在报告里显式说明，不静默处理）：
//!   - `workspace_scope='workspace'` 的 `space_id` 按 `spaces.user_id` 隔离。目标账号
//!     有同名同路径的空间就重映射，没有就降级为 `task` 作用域（`cwds` 原样保留，
//!     任务照旧在同一目录跑，只是不再挂在该空间下）。
//!   - `next_run_at` 沿用原值：未来时间点照常跑；已过期的会被客户端按 missed 跳过并
//!     重算下一次（`computeNextRunAtAfterSkippedOccurrence`），这里不自己实现 RRULE。
//!   - 运行历史（`automation_runs` / `automation_runtime_state`）不跟着走：复制出的
//!     新 id 天然干净；转移则保留原 id，历史里的旧会话仍属于源账号。

use std::path::{Path, PathBuf};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::modules::config::{atomic_write, backup_dir, now_ms, store_dir, utc_iso};
use crate::modules::session::{open_db, table_exists};
use crate::modules::session_backup;
use crate::modules::variant::WbVariant;

/// 任务表名（客户端 5.x 起固定）。
const AUTOMATIONS_TABLE: &str = "automations";
/// 复制关系的持久化文件：按 sourceId 快速识别同一来源的副本；跨 id 的兜底靠定义指纹。
const LINKS_FILE: &str = "automation-links.json";
/// 不参与「同一任务」判定的列：切换/运行必然变化，或用户可单独调整。
///
/// `status` 也不比：目标账号有同定义的暂停任务时，再复制一份启用任务只会制造重复，
/// 应让用户自行决定恢复哪个。`space_id` / `workspace_scope` 在跨账号映射时会降级，
/// 同样不能拿来当身份。
const VOLATILE_COLUMNS: &[&str] = &[
    "id",
    "owner_user_id",
    "owner_status",
    "owner_source",
    "created_at",
    "updated_at",
    "last_run_at",
    "next_run_at",
    "deleted_at",
    "status",
    "workspace_scope",
    "space_id",
];
/// 写进去的归属状态：`confirmed` 才会让客户端的归属校正跳过该行。
const OWNER_STATUS_CONFIRMED: &str = "confirmed";
/// 归属来源标记（客户端只把它当证据文字，不影响判定）。
const OWNER_SOURCE_COPY: &str = "switch_copy";
const OWNER_SOURCE_TRANSFER: &str = "switch_transfer";
/// 无空间（detached）的哨兵值。
const SPACE_DETACHED: &str = "-1";
/// 任务自带群机器人通知块的标记：指纹计算时会整段剥掉，避免注入通知后认不出同一任务。
const WECOM_NOTIFY_START: &str = "<!-- wb-switch-wecom-notify:v1 -->";
const WECOM_NOTIFY_END: &str = "<!-- /wb-switch-wecom-notify -->";
/// 企业微信群机器人 Webhook 配置文件（放在 switch 工具自己的 store 目录）。
const WECOM_WEBHOOK_FILE: &str = "wecom-webhook.json";

// ---------------------------------------------------------------------------
// 选择项
// ---------------------------------------------------------------------------

/// 单个任务的处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationAction {
    /// 复制：新 id、新归属，源账号保留。
    Copy,
    /// 转移：改归属，源账号失去。
    Transfer,
}

impl AutomationAction {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "copy" => Some(Self::Copy),
            "transfer" => Some(Self::Transfer),
            _ => None,
        }
    }
}

/// 一条勾选：任务 id + 处理方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationSelection {
    pub id: String,
    pub action: AutomationAction,
}

impl AutomationSelection {
    fn parse(value: &Value) -> Result<Self, String> {
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "automationSelections 的每一项都必须带 id".to_string())?;
        let action_raw = value
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("任务 {id} 缺少 action（copy / transfer）"))?;
        let action = AutomationAction::parse(action_raw)
            .ok_or_else(|| format!("任务 {id} 的 action 非法：{action_raw}"))?;
        Ok(Self { id, action })
    }
}

/// 解析 `automationSelections`（缺省 / null 视为没勾选）。
///
/// 同一 id 重复出现时只保留第一次：一次切号里同一个任务不可能既复制又转移。
pub fn parse_automation_selections(
    value: Option<&Value>,
) -> Result<Vec<AutomationSelection>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err("automationSelections 必须是数组".to_string());
    };
    let mut out: Vec<AutomationSelection> = Vec::with_capacity(items.len());
    for item in items {
        let parsed = AutomationSelection::parse(item)?;
        if out.iter().any(|seen| seen.id == parsed.id) {
            continue;
        }
        out.push(parsed);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 可见性
// ---------------------------------------------------------------------------

/// 任务对某个 uid 是否可见（与客户端 `isAutomationVisibleToUser` 同口径）。
///
/// `confirmed` / `inferred` 是「已认领」，只有归属账号看得见；其余状态视为未认领，
/// 任何账号都看得见。
pub fn is_visible_to(owner_uid: Option<&str>, owner_status: Option<&str>, uid: &str) -> bool {
    let status = owner_status.unwrap_or("legacy_unassigned").trim();
    if status == "confirmed" || status == "inferred" {
        return owner_uid
            .map(|v| v.trim() == uid)
            .unwrap_or(false);
    }
    true
}

// ---------------------------------------------------------------------------
// 数据库读取
// ---------------------------------------------------------------------------

fn db_path(variant: WbVariant) -> PathBuf {
    variant.data_root().join("workbuddy.db")
}

/// 一行任务的列名 + 值（按表自身列序）。
///
/// 用动态列而不是固定 SELECT：客户端加列时本工具不需要跟着改，写回时只碰认识的列。
type RowPairs = Vec<(String, SqlValue)>;

fn read_row(conn: &Connection, id: &str) -> Result<Option<RowPairs>, String> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {AUTOMATIONS_TABLE} WHERE id = ?1"))
        .map_err(|e| format!("定时任务表读取失败：{e}"))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt
        .query(rusqlite::params![id])
        .map_err(|e| format!("定时任务查询失败：{e}"))?;
    let Some(row) = rows.next().map_err(|e| format!("定时任务读取失败：{e}"))? else {
        return Ok(None);
    };
    let mut out: RowPairs = Vec::with_capacity(cols.len());
    for (index, col) in cols.into_iter().enumerate() {
        let value = row
            .get::<_, SqlValue>(index)
            .unwrap_or(SqlValue::Null);
        out.push((col, value));
    }
    Ok(Some(out))
}

fn text_of<'a>(row: &'a RowPairs, key: &str) -> Option<&'a str> {
    row.iter()
        .find(|(name, _)| name == key)
        .and_then(|(_, value)| match value {
            SqlValue::Text(text) => Some(text.as_str()),
            _ => None,
        })
}

fn int_of(row: &RowPairs, key: &str) -> Option<i64> {
    row.iter()
        .find(|(name, _)| name == key)
        .and_then(|(_, value)| match value {
            SqlValue::Integer(number) => Some(*number),
            _ => None,
        })
}

fn has_column(row: &RowPairs, key: &str) -> bool {
    row.iter().any(|(name, _)| name == key)
}

/// 是否已删除：客户端把 `deleted_at` 写成时间戳，未删为 NULL（或历史的 0 / -1）。
fn is_deleted(row: &RowPairs) -> bool {
    row.iter()
        .find(|(name, _)| name == "deleted_at")
        .map(|(_, value)| match value {
            SqlValue::Null => false,
            SqlValue::Integer(number) => *number > 0,
            _ => false,
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 任务定义指纹（跨 id 去重）
// ---------------------------------------------------------------------------

fn fingerprint_text(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => String::new(),
        SqlValue::Integer(number) => number.to_string(),
        SqlValue::Real(number) => number.to_string(),
        SqlValue::Text(text) => text.trim().to_string(),
        SqlValue::Blob(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    }
}

/// 剥掉任务自带通知块：通知是「随任务走的投递方式」，不算任务定义本身。
fn strip_wecom_notify_block(prompt: &str) -> String {
    let mut text = prompt.to_string();
    while let Some(start) = text.find(WECOM_NOTIFY_START) {
        let Some(rel_end) = text[start..].find(WECOM_NOTIFY_END) else {
            break;
        };
        let end = start + rel_end + WECOM_NOTIFY_END.len();
        text.replace_range(start..end, "");
    }
    text.trim().to_string()
}

/// 从 switch 的 store 目录读取企业微信群机器人 Webhook。
fn load_wecom_webhook() -> Option<String> {
    let path = store_dir().join(WECOM_WEBHOOK_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("webhook")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn valid_wecom_webhook(webhook: &str) -> bool {
    webhook.starts_with("https://qyapi.weixin.qq.com/cgi-bin/webhook/send")
        && webhook.contains("key=")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// 生成「任务自带通知」块：Webhook 和调用命令都写在任务 prompt 里，换账号照样发。
fn wecom_notify_prompt(name: &str, webhook: &str) -> String {
    let payload = json!({
        "msgtype": "text",
        "text": { "content": format!("【{name}】{{SUMMARY}}") },
    })
    .to_string();
    format!(
        "{WECOM_NOTIFY_START}\n\
         【任务自带通知：企业微信群机器人】任务完成后（成功或失败都要），把本次结果摘要\
         通过下面的 Webhook 推送到企业微信群；摘要不超过 500 字，替换 {{SUMMARY}} 后发送：\n\
         curl -sS -X POST -H 'Content-Type: application/json' -d {} {}\n\
         返回 errcode=0 才算推送成功；非 0 要在任务结果里写明错误。\n\
         {WECOM_NOTIFY_END}",
        shell_quote(&payload),
        shell_quote(webhook),
    )
}

/// 给带通知开关的任务写入群机器人 Webhook，返回给用户的说明。
fn add_wecom_notification(
    row: &RowPairs,
    name: &str,
    webhook: Option<&str>,
    overrides: &mut Vec<(String, SqlValue)>,
) -> Option<String> {
    if int_of(row, "push_to_wecom_bot").unwrap_or(0) == 0
        && int_of(row, "push_to_wechat").unwrap_or(0) == 0
    {
        return None;
    }
    let Some(webhook) = webhook.map(str::trim).filter(|text| !text.is_empty()) else {
        return Some(
            "该任务开启了通知，但未配置企业微信群机器人 Webhook，未写入任务级通知".to_string(),
        );
    };
    if !valid_wecom_webhook(webhook) {
        return Some("企业微信群机器人 Webhook 格式不正确，未写入任务级通知".to_string());
    }
    let old = strip_wecom_notify_block(text_of(row, "prompt").unwrap_or_default());
    let new_prompt = format!("{}\n\n{}", old.trim_end(), wecom_notify_prompt(name, webhook));
    set_if_present(row, overrides, "prompt", SqlValue::Text(new_prompt));
    Some("已把企业微信群机器人 Webhook 写入任务，通知随任务走".to_string())
}

/// 已有副本缺任务级通知时，就地补写 Webhook，不新增任务行。
///
/// 老副本是切号前生成的，prompt 里没有通知块；同一任务再次被勾选时顺手升级它，
/// 这样不用先删旧任务再复制一遍，也不会破坏去重。
fn upgrade_existing_notification(
    conn: &Connection,
    existing_id: &str,
    source: &RowPairs,
    webhook: Option<&str>,
) -> Result<bool, String> {
    if int_of(source, "push_to_wecom_bot").unwrap_or(0) == 0
        && int_of(source, "push_to_wechat").unwrap_or(0) == 0
    {
        return Ok(false);
    }
    let Some(webhook) = webhook.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(false);
    };
    if !valid_wecom_webhook(webhook) {
        return Ok(false);
    }
    let Some(existing) = read_row(conn, existing_id)? else {
        return Ok(false);
    };
    if text_of(&existing, "prompt")
        .unwrap_or_default()
        .contains(WECOM_NOTIFY_START)
    {
        return Ok(false);
    }
    let old = strip_wecom_notify_block(text_of(&existing, "prompt").unwrap_or_default());
    let name = text_of(&existing, "name").unwrap_or_default();
    let new_prompt = format!("{}\n\n{}", old.trim_end(), wecom_notify_prompt(name, webhook));
    conn.execute(
        &format!("UPDATE {AUTOMATIONS_TABLE} SET prompt = ?1 WHERE id = ?2"),
        rusqlite::params![new_prompt, existing_id],
    )
    .map_err(|e| format!("任务通知补写失败：{e}"))?;
    Ok(true)
}

/// 任务定义指纹：只比「要做什么、何时做、用什么配置做」。
///
/// 用全列的动态交集而不是写死几个字段：客户端加列时自动纳入判定，不会因为漏字段
/// 把两个不同任务认成同一个。带长度前缀是为了避免不同列的值拼接后产生歧义。
/// 通知块单独剥掉：它是投递方式，不属于任务身份。
fn definition_fingerprint(row: &RowPairs) -> String {
    let mut parts: Vec<(String, String)> = row
        .iter()
        .filter(|(name, _)| !VOLATILE_COLUMNS.contains(&name.as_str()))
        .map(|(name, value)| {
            let text = match value {
                SqlValue::Text(text) if name == "prompt" => strip_wecom_notify_block(text),
                _ => fingerprint_text(value),
            };
            (name.clone(), text)
        })
        .collect();
    parts.sort_by(|left, right| left.0.cmp(&right.0));
    parts
        .into_iter()
        .map(|(name, value)| format!("{}|{}|{}|{};", name.len(), name, value.len(), value))
        .collect()
}

/// 目标账号里是否已有等价任务，返回 `(id, name)`。
///
/// 已删除的、对目标账号不可见的、以及源任务自身都不算。源任务在 `legacy_unassigned`
/// 状态下对目标账号也可见，必须显式排除，否则会把自己当成「目标已有」而跳过。
fn find_equivalent_target(
    conn: &Connection,
    row: &RowPairs,
    source_id: &str,
    target_uid: &str,
) -> Result<Option<(String, String)>, String> {
    let fingerprint = definition_fingerprint(row);
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {AUTOMATIONS_TABLE}"))
        .map_err(|e| format!("定时任务表读取失败：{e}"))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut pairs: RowPairs = Vec::with_capacity(cols.len());
        for (index, col) in cols.iter().enumerate() {
            pairs.push((
                col.clone(),
                row.get::<_, SqlValue>(index).unwrap_or(SqlValue::Null),
            ));
        }
        if is_deleted(&pairs) || text_of(&pairs, "id") == Some(source_id) {
            continue;
        }
        if !is_visible_to(
            text_of(&pairs, "owner_user_id"),
            text_of(&pairs, "owner_status"),
            target_uid,
        ) {
            continue;
        }
        if definition_fingerprint(&pairs) == fingerprint {
            return Ok(Some((
                text_of(&pairs, "id").unwrap_or_default().to_string(),
                text_of(&pairs, "name").unwrap_or_default().to_string(),
            )));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// 列表
// ---------------------------------------------------------------------------

/// 当前账号可见的任务列表（`GET /api/automations`）。
///
/// 返回 `{ current, automations: [...] }`；没有登录态或没有任务表时返回空列表，
/// 与 `list_sessions` 的容错口径一致。
pub fn list_automations(variant: WbVariant) -> Value {
    list_automations_for_uid(variant, None)
}

/// 指定 uid 可见的任务列表；`uid=None` 时取当前登录态。
///
/// 每项带 `definitionKey`，与落库去重用的是同一指纹：切号弹窗据此把「目标账号已有」
/// 的任务默认不勾选，避免先勾上再在结果里报「跳过」。
pub fn list_automations_for_uid(variant: WbVariant, uid: Option<&str>) -> Value {
    let uid = uid
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| crate::modules::session::current_user_uid(variant))
        .unwrap_or_default();
    if uid.is_empty() {
        return json!({ "current": Value::Null, "automations": [] });
    }
    let items = automations_for_user(variant, &uid).unwrap_or_else(|_| Vec::new());
    json!({ "current": uid, "automations": items })
}

fn automations_for_user(variant: WbVariant, uid: &str) -> Result<Vec<Value>, String> {
    let path = db_path(variant);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let Some(conn) = open_db(&path, true) else {
        return Err("WorkBuddy 数据库无法打开".to_string());
    };
    if !table_exists(&conn, AUTOMATIONS_TABLE) {
        return Ok(Vec::new());
    }
    let webhook = load_wecom_webhook().filter(|value| valid_wecom_webhook(value));
    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM {AUTOMATIONS_TABLE} \
             ORDER BY CASE status WHEN 'ACTIVE' THEN 0 ELSE 1 END, name"
        ))
        .map_err(|e| format!("定时任务表读取失败：{e}"))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut pairs: RowPairs = Vec::with_capacity(cols.len());
        for (index, col) in cols.iter().enumerate() {
            let value = row
                .get::<_, SqlValue>(index)
                .unwrap_or(SqlValue::Null);
            pairs.push((col.clone(), value));
        }
        if is_deleted(&pairs) {
            continue;
        }
        if !is_visible_to(
            text_of(&pairs, "owner_user_id"),
            text_of(&pairs, "owner_status"),
            uid,
        ) {
            continue;
        }
        out.push(json!({
            "id": text_of(&pairs, "id").unwrap_or_default(),
            "definitionKey": definition_fingerprint(&pairs),
            // 带通知开关、但 prompt 还没有任务级 Webhook 的老任务：切号时补写一次。
            "needsNotificationUpgrade": webhook.is_some()
                && (int_of(&pairs, "push_to_wecom_bot").unwrap_or(0) > 0
                    || int_of(&pairs, "push_to_wechat").unwrap_or(0) > 0)
                && !text_of(&pairs, "prompt")
                    .unwrap_or_default()
                    .contains(WECOM_NOTIFY_START),
            "name": text_of(&pairs, "name").unwrap_or_default(),
            "prompt": text_of(&pairs, "prompt").unwrap_or_default(),
            "status": text_of(&pairs, "status").unwrap_or_default(),
            "scheduleType": text_of(&pairs, "schedule_type").unwrap_or("recurring"),
            "rrule": text_of(&pairs, "rrule").unwrap_or_default(),
            "scheduledAt": text_of(&pairs, "scheduled_at"),
            "nextRunAt": int_of(&pairs, "next_run_at"),
            "lastRunAt": int_of(&pairs, "last_run_at"),
            "cwd": first_cwd(text_of(&pairs, "cwds")),
            "workspaceScope": text_of(&pairs, "workspace_scope"),
            "spaceId": text_of(&pairs, "space_id"),
        }));
    }
    Ok(out)
}

/// `cwds` 是 JSON 数组字符串，列表只展示第一个目录。
fn first_cwd(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.as_str())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// 复制关系（幂等）
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct LinkStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    links: Vec<LinkEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct LinkEntry {
    #[serde(rename = "sourceId")]
    source_id: String,
    #[serde(rename = "targetUid")]
    target_uid: String,
    #[serde(rename = "targetId")]
    target_id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "updatedAt", default)]
    updated_at: i64,
}

fn links_path_at(store_root: &Path) -> PathBuf {
    store_root.join(LINKS_FILE)
}

/// 读取复制关系；文件缺失或损坏都当空表（复制关系只影响去重，不该阻断切号）。
fn load_links_at(store_root: &Path) -> LinkStore {
    let Ok(text) = std::fs::read_to_string(links_path_at(store_root)) else {
        return LinkStore::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_links_at(store_root: &Path, store: &LinkStore) -> Result<(), String> {
    let path = links_path_at(store_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("复制关系目录创建失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    atomic_write(&path, &text).map_err(|e| format!("复制关系保存失败：{e}"))
}

// ---------------------------------------------------------------------------
// 空间作用域
// ---------------------------------------------------------------------------

/// 复制到目标账号后的空间处置。
#[derive(Clone, Debug, PartialEq, Eq)]
enum SpaceOutcome {
    /// 无空间或空间本就属于目标账号：原值照抄。
    Keep,
    /// 目标账号有同名同路径的空间：改成那个 id。
    Remap(String),
    /// 目标账号没有对应空间：降级为 task 作用域（清空 space_id）。
    Downgrade,
}

impl SpaceOutcome {
    fn note(&self) -> Option<String> {
        match self {
            Self::Keep => None,
            Self::Remap(_) => Some("已映射到目标账号的同名空间".to_string()),
            Self::Downgrade => Some(
                "目标账号没有该空间，已降级为任务作用域（仍在原目录运行）".to_string(),
            ),
        }
    }
}

/// `spaces` 表按 `user_id` 隔离，直接照抄 `space_id` 会让任务挂到别人的空间上。
fn resolve_space(conn: &Connection, row: &RowPairs, target_uid: &str) -> Result<SpaceOutcome, String> {
    let scope = text_of(row, "workspace_scope").unwrap_or_default();
    let space_id = text_of(row, "space_id").unwrap_or_default().trim().to_string();
    if scope != "workspace" || space_id.is_empty() || space_id == SPACE_DETACHED {
        return Ok(SpaceOutcome::Keep);
    }
    if !table_exists(conn, "spaces") {
        return Ok(SpaceOutcome::Downgrade);
    }
    let source_space = conn
        .query_row(
            "SELECT user_id, primary_folder_path FROM spaces WHERE id = ?1",
            rusqlite::params![space_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .ok();
    let Some((owner, folder)) = source_space else {
        // 源空间已被删除：留着一个不存在的 id 只会让任务挂在孤儿空间上。
        return Ok(SpaceOutcome::Downgrade);
    };
    if owner.as_deref().map(str::trim) == Some(target_uid) {
        return Ok(SpaceOutcome::Keep);
    }

    let folder = folder.unwrap_or_default();
    if !folder.trim().is_empty() {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM spaces \
                 WHERE user_id = ?1 AND deleted_at < 0 AND status = 'active' \
                   AND primary_folder_path = ?2 \
                 ORDER BY created_at LIMIT 1",
                rusqlite::params![target_uid, folder],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok(SpaceOutcome::Remap(id));
        }
    }
    // 同名空间 + 空路径的情况也认一次：路径为空时按名称匹配。
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM spaces WHERE id = ?1",
            rusqlite::params![space_id],
            |row| row.get(0),
        )
        .ok();
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM spaces \
                 WHERE user_id = ?1 AND deleted_at < 0 AND status = 'active' AND name = ?2 \
                 ORDER BY created_at LIMIT 1",
                rusqlite::params![target_uid, name],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok(SpaceOutcome::Remap(id));
        }
    }
    Ok(SpaceOutcome::Downgrade)
}

/// 空间相关的列覆盖（只覆盖表里真实存在的列）。
fn space_overrides(row: &RowPairs, outcome: &SpaceOutcome) -> Vec<(String, SqlValue)> {
    match outcome {
        SpaceOutcome::Keep => Vec::new(),
        SpaceOutcome::Remap(id) => {
            let mut out = Vec::new();
            if has_column(row, "space_id") {
                out.push(("space_id".to_string(), SqlValue::Text(id.clone())));
            }
            out
        }
        SpaceOutcome::Downgrade => {
            let mut out = Vec::new();
            if has_column(row, "space_id") {
                out.push(("space_id".to_string(), SqlValue::Null));
            }
            if has_column(row, "workspace_scope") {
                out.push((
                    "workspace_scope".to_string(),
                    SqlValue::Text("task".to_string()),
                ));
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/// 插入一份副本（列取交集，覆盖项优先）。
///
/// 用 INSERT 而不是 INSERT OR REPLACE：新 id 撞库时宁可失败，也不能悄悄覆盖既有任务。
fn insert_copy(
    conn: &Connection,
    row: &RowPairs,
    overrides: &[(String, SqlValue)],
) -> Result<(), String> {
    let mut cols: Vec<&str> = Vec::with_capacity(row.len());
    let mut vals: Vec<SqlValue> = Vec::with_capacity(row.len());
    for (name, value) in row {
        cols.push(name.as_str());
        let value = overrides
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| value.clone());
        vals.push(value);
    }
    let colnames = cols
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=cols.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO {AUTOMATIONS_TABLE} ({colnames}) VALUES ({placeholders})");
    conn.execute(&sql, rusqlite::params_from_iter(vals.iter()))
        .map_err(|e| format!("任务记录写入失败：{e}"))?;
    Ok(())
}

/// 只改归属（转移）：其余字段一律不动，历史运行记录继续挂在原 id 上。
fn update_owner(
    conn: &Connection,
    id: &str,
    overrides: &[(String, SqlValue)],
) -> Result<(), String> {
    if overrides.is_empty() {
        return Ok(());
    }
    let sets = overrides
        .iter()
        .enumerate()
        .map(|(index, (name, _))| format!("\"{name}\" = ?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("UPDATE {AUTOMATIONS_TABLE} SET {sets} WHERE id = ?{}", overrides.len() + 1);
    let mut vals: Vec<&SqlValue> = overrides.iter().map(|(_, value)| value).collect();
    let id_value = SqlValue::Text(id.to_string());
    vals.push(&id_value);
    let changed = conn
        .execute(&sql, rusqlite::params_from_iter(vals))
        .map_err(|e| format!("任务归属更新失败：{e}"))?;
    if changed == 0 {
        return Err("任务不存在或已被删除".to_string());
    }
    Ok(())
}

/// 写后校验：归属必须真的变成目标 uid（不能只看 execute 的返回值）。
fn verify_owner(conn: &Connection, id: &str, target_uid: &str) -> Result<(), String> {
    let owner: Option<Option<String>> = conn
        .query_row(
            &format!("SELECT owner_user_id FROM {AUTOMATIONS_TABLE} WHERE id = ?1"),
            rusqlite::params![id],
            |row| row.get(0),
        )
        .ok();
    match owner {
        Some(Some(owner)) if owner.trim() == target_uid => Ok(()),
        Some(Some(owner)) => Err(format!("归属校验失败：期望 {target_uid}，实际 {owner}")),
        Some(None) => Err("归属校验失败：写入后归属为空".to_string()),
        None => Err("归属校验失败：写入后找不到该任务".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 备份
// ---------------------------------------------------------------------------

/// 写入前备份整库（含 WAL / SHM）。切号时 WorkBuddy 已关闭，直接复制即可。
fn backup_db_at(db: &Path, backup_root: &Path) -> Result<Option<PathBuf>, String> {
    if !db.is_file() {
        return Ok(None);
    }
    let dir = backup_root.join(format!("automations-{}", utc_iso()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("备份目录创建失败：{e}"))?;
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", db.to_string_lossy()));
        if !src.is_file() {
            continue;
        }
        let dest = dir.join(format!("workbuddy.db{suffix}"));
        std::fs::copy(&src, &dest).map_err(|e| format!("备份 {suffix} 失败：{e}"))?;
    }
    Ok(Some(dir))
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 一次任务处理的成功类别。
enum Outcome {
    Copied { target_id: String },
    Transferred,
    Skipped(String),
}

/// 单个任务的处理结果：类别 + 展示名 + 需要告知用户的事项。
struct Applied {
    name: String,
    outcome: Outcome,
    notes: Vec<String>,
}

/// 把勾选的任务落到目标账号。
///
/// 返回 `None` 表示本次没勾选（调用方不写报告）；结构性失败（库打不开、备份失败）
/// 才返回 `Err`，单个任务的失败进报告的 `errors`，不阻断切号——与会话复制同口径。
pub fn apply_for_switch(
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
    selections: &[AutomationSelection],
) -> Result<Option<Value>, String> {
    let webhook = load_wecom_webhook();
    apply_at(
        &db_path(variant),
        &store_dir(),
        &backup_dir(),
        source_uid,
        target_uid,
        webhook.as_deref(),
        selections,
    )
}

/// [`apply_for_switch`] 的可测实现：显式传入库路径、复制关系根与备份根。
fn apply_at(
    db: &Path,
    store_root: &Path,
    backup_root: &Path,
    source_uid: &str,
    target_uid: &str,
    webhook: Option<&str>,
    selections: &[AutomationSelection],
) -> Result<Option<Value>, String> {
    if selections.is_empty() {
        return Ok(None);
    }
    let source_uid = source_uid.trim();
    let target_uid = target_uid.trim();
    if source_uid.is_empty() {
        return Err("未检测到当前登录账号，无法处理定时任务".to_string());
    }
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法处理定时任务".to_string());
    }
    if source_uid == target_uid {
        return Err("目标账号与当前账号相同，未处理定时任务".to_string());
    }

    if !db.is_file() {
        return Err("WorkBuddy 数据库不存在，未处理定时任务".to_string());
    }
    let backup = backup_db_at(db, backup_root)?;

    let mut conn = open_db(db, false).ok_or("WorkBuddy 数据库无法打开，未处理定时任务")?;
    if !table_exists(&conn, AUTOMATIONS_TABLE) {
        return Err("当前客户端版本没有定时任务表，未处理定时任务".to_string());
    }
    session_backup::ensure_full_synchronous(&conn)?;

    let mut copied: Vec<Value> = Vec::new();
    let mut transferred: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    let mut new_links: Vec<LinkEntry> = Vec::new();
    let mut links = load_links_at(store_root);

    {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("数据库写事务无法开始：{e}"))?;

        for selection in selections {
            match apply_one(
                &tx,
                selection,
                source_uid,
                target_uid,
                &links,
                &mut new_links,
                webhook,
            ) {
                Ok(applied) => {
                    for note in applied.notes {
                        notes.push(json!({
                            "sourceId": selection.id,
                            "name": applied.name,
                            "message": note,
                        }));
                    }
                    match applied.outcome {
                        Outcome::Copied { target_id } => {
                            copied.push(json!({
                                "sourceId": selection.id,
                                "targetId": target_id,
                                "name": applied.name,
                            }));
                        }
                        Outcome::Transferred => {
                            transferred.push(json!({
                                "sourceId": selection.id,
                                "name": applied.name,
                            }));
                        }
                        Outcome::Skipped(message) => {
                            skipped.push(json!({
                                "sourceId": selection.id,
                                "name": applied.name,
                                "message": message,
                            }));
                        }
                    }
                }
                Err(error) => {
                    errors.push(json!({
                        "sourceId": selection.id,
                        "error": error,
                    }));
                }
            }
        }
        tx.commit()
            .map_err(|e| format!("任务记录提交失败：{e}"))?;
    }

    // 复制关系只在提交成功后落盘：事务回滚时不能留下「已复制」的假记录。
    if !new_links.is_empty() {
        links.links.extend(new_links);
        if let Err(error) = save_links_at(store_root, &links) {
            // 关系文件写不进去只影响下次去重，不影响本次结果，用 note 说清楚。
            notes.push(json!({
                "message": format!("复制关系保存失败（下次可能重复复制）：{error}"),
            }));
        }
    }

    Ok(Some(json!({
        "copied": copied,
        "transferred": transferred,
        "skipped": skipped,
        "errors": errors,
        "notes": notes,
        "backup": backup.map(|p| p.to_string_lossy().to_string()),
    })))
}

fn apply_one(
    conn: &Connection,
    selection: &AutomationSelection,
    source_uid: &str,
    target_uid: &str,
    links: &LinkStore,
    new_links: &mut Vec<LinkEntry>,
    webhook: Option<&str>,
) -> Result<Applied, String> {
    let Some(row) = read_row(conn, &selection.id)? else {
        return Err("任务不存在或已被删除".to_string());
    };
    if is_deleted(&row) {
        return Err("任务已删除".to_string());
    }
    let name = text_of(&row, "name").unwrap_or_default().to_string();
    if !is_visible_to(
        text_of(&row, "owner_user_id"),
        text_of(&row, "owner_status"),
        source_uid,
    ) {
        return Err("该任务归属其他账号，不能处理".to_string());
    }

    let space = resolve_space(conn, &row, target_uid)?;
    let now = now_ms();
    // 跨 id 兜底：往返切换后同一任务每次都换个 id 出现在源账号，links 认不出来。
    let equivalent = find_equivalent_target(conn, &row, &selection.id, target_uid)?;
    // 空间被降级/重映射、一次性任务已过期，都要让用户看见，不能悄悄改掉。
    let mut notes: Vec<String> = Vec::new();
    if let Some(note) = space.note() {
        notes.push(note);
    }
    if let Some(note) = once_expired_note(&row) {
        notes.push(note);
    }
    match selection.action {
        AutomationAction::Copy => {
            // 同一来源已经复制给同一目标：不再产生第二份。
            if let Some(link) = links
                .links
                .iter()
                .find(|link| link.source_id == selection.id && link.target_uid == target_uid)
            {
                let existing = read_row(conn, &link.target_id)?;
                let still_there = existing
                    .as_ref()
                    .map(|row| {
                        !is_deleted(row)
                            && text_of(row, "owner_user_id").map(str::trim) == Some(target_uid)
                    })
                    .unwrap_or(false);
                if still_there {
                    let upgraded =
                        upgrade_existing_notification(conn, &link.target_id, &row, webhook)?;
                    let label = if link.name.is_empty() {
                        link.target_id.as_str()
                    } else {
                        link.name.as_str()
                    };
                    return Ok(Applied {
                        name,
                        outcome: Outcome::Skipped(if upgraded {
                            format!("已复制过该任务（{label}），已补写企业微信群机器人通知")
                        } else {
                            format!("已复制过该任务（{label}）")
                        }),
                        notes,
                    });
                }
            }
            // links 只认同一 sourceId；换过 id 的往返副本靠定义指纹在这里拦下。
            if let Some((existing_id, existing_name)) = &equivalent {
                let already_linked = links
                    .links
                    .iter()
                    .any(|link| link.source_id == selection.id && link.target_uid == target_uid);
                if !already_linked {
                    // 补一条链接，下次同一 sourceId 可直接走快速路径。
                    new_links.push(LinkEntry {
                        source_id: selection.id.clone(),
                        target_uid: target_uid.to_string(),
                        target_id: existing_id.clone(),
                        name: existing_name.clone(),
                        updated_at: now,
                    });
                }
                let label = if existing_name.trim().is_empty() {
                    existing_id.as_str()
                } else {
                    existing_name.as_str()
                };
                let upgraded =
                    upgrade_existing_notification(conn, existing_id, &row, webhook)?;
                return Ok(Applied {
                    name,
                    outcome: Outcome::Skipped(if upgraded {
                        format!("目标已有相同任务（{label}），已补写企业微信群机器人通知")
                    } else {
                        format!("目标账号已有相同任务（{label}），未新增副本")
                    }),
                    notes,
                });
            }

            let new_id = uuid::Uuid::new_v4().to_string();
            let mut overrides: Vec<(String, SqlValue)> = Vec::new();
            if let Some(note) = add_wecom_notification(&row, &name, webhook, &mut overrides) {
                notes.push(note);
            }
            set_if_present(&row, &mut overrides, "id", SqlValue::Text(new_id.clone()));
            set_if_present(
                &row,
                &mut overrides,
                "owner_user_id",
                SqlValue::Text(target_uid.to_string()),
            );
            set_if_present(
                &row,
                &mut overrides,
                "owner_status",
                SqlValue::Text(OWNER_STATUS_CONFIRMED.to_string()),
            );
            set_if_present(
                &row,
                &mut overrides,
                "owner_source",
                SqlValue::Text(OWNER_SOURCE_COPY.to_string()),
            );
            set_if_present(&row, &mut overrides, "created_at", SqlValue::Integer(now));
            set_if_present(&row, &mut overrides, "updated_at", SqlValue::Integer(now));
            set_if_present(&row, &mut overrides, "last_run_at", SqlValue::Null);
            set_if_present(&row, &mut overrides, "deleted_at", SqlValue::Null);
            overrides.extend(space_overrides(&row, &space));

            insert_copy(conn, &row, &overrides)?;
            verify_owner(conn, &new_id, target_uid)?;

            let entry = LinkEntry {
                source_id: selection.id.clone(),
                target_uid: target_uid.to_string(),
                target_id: new_id.clone(),
                name: name.clone(),
                updated_at: now,
            };
            // 只记到 new_links：提交成功后才并入落盘表，避免回滚时留下假记录。
            new_links.push(entry);
            Ok(Applied {
                name,
                outcome: Outcome::Copied { target_id: new_id },
                notes,
            })
        }
        AutomationAction::Transfer => {
            if text_of(&row, "owner_user_id").map(str::trim) == Some(target_uid) {
                return Ok(Applied {
                    name,
                    outcome: Outcome::Skipped("该任务已属于目标账号".to_string()),
                    notes,
                });
            }
            // 转移虽不新增行，但会在目标账号凑出两份一样的任务，同样拦下。
            if let Some((existing_id, existing_name)) = &equivalent {
                let label = if existing_name.trim().is_empty() {
                    existing_id.as_str()
                } else {
                    existing_name.as_str()
                };
                return Ok(Applied {
                    name,
                    outcome: Outcome::Skipped(format!(
                        "目标账号已有相同任务（{label}），未转移以免重复"
                    )),
                    notes,
                });
            }
            let mut overrides: Vec<(String, SqlValue)> = Vec::new();
            if let Some(note) = add_wecom_notification(&row, &name, webhook, &mut overrides) {
                notes.push(note);
            }
            set_if_present(
                &row,
                &mut overrides,
                "owner_user_id",
                SqlValue::Text(target_uid.to_string()),
            );
            set_if_present(
                &row,
                &mut overrides,
                "owner_status",
                SqlValue::Text(OWNER_STATUS_CONFIRMED.to_string()),
            );
            set_if_present(
                &row,
                &mut overrides,
                "owner_source",
                SqlValue::Text(OWNER_SOURCE_TRANSFER.to_string()),
            );
            set_if_present(&row, &mut overrides, "updated_at", SqlValue::Integer(now));
            overrides.extend(space_overrides(&row, &space));

            update_owner(conn, &selection.id, &overrides)?;
            verify_owner(conn, &selection.id, target_uid)?;
            Ok(Applied {
                name,
                outcome: Outcome::Transferred,
                notes,
            })
        }
    }
}

/// 只覆盖表里真实存在的列：客户端旧版少列时不能凭空写不存在的字段。
fn set_if_present(
    row: &RowPairs,
    overrides: &mut Vec<(String, SqlValue)>,
    key: &str,
    value: SqlValue,
) {
    if has_column(row, key) {
        overrides.push((key.to_string(), value));
    }
}

/// 一次性任务的计划时间已过：复制后客户端会直接判定过期，不会执行。
fn once_expired_note(row: &RowPairs) -> Option<String> {
    if text_of(row, "schedule_type") != Some("once") {
        return None;
    }
    let raw = text_of(row, "scheduled_at")?.trim().to_string();
    let naive = chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    (naive < chrono::Local::now().naive_local())
        .then(|| "一次性任务的计划时间已过，复制后不会执行".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WEBHOOK: &str =
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=test-key-1234567890";

    #[test]
    fn visibility_matches_client_rule() {
        // 已认领：只有归属账号看得见。
        assert!(is_visible_to(Some("u1"), Some("confirmed"), "u1"));
        assert!(!is_visible_to(Some("u1"), Some("confirmed"), "u2"));
        assert!(is_visible_to(Some("u1"), Some("inferred"), "u1"));
        assert!(!is_visible_to(Some("u1"), Some("inferred"), "u2"));
        // 未认领：谁都能看见。
        assert!(is_visible_to(None, Some("legacy_unassigned"), "u2"));
        assert!(is_visible_to(Some("u1"), Some("conflict"), "u2"));
        assert!(is_visible_to(None, None, "u2"));
        // 有归属但状态已认领、uid 为空：不可见（客户端同样要求 both）。
        assert!(!is_visible_to(None, Some("confirmed"), "u1"));
    }

    /// 列表对外暴露的 definitionKey 必须与去重判据一致：忽略运行字段、跟随定义变化。
    #[test]
    fn definition_key_ignores_runtime_fields_but_tracks_definition() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.insert("b1", "任务", Some("u2"), "confirmed", None, None, "recurring", None, None);
        assert_eq!(
            definition_fingerprint(&temp.row("a1").unwrap()),
            definition_fingerprint(&temp.row("b1").unwrap())
        );

        temp.conn()
            .execute(
                "UPDATE automations SET status = 'PAUSED', next_run_at = ?1, \
                 owner_user_id = 'u3' WHERE id = 'b1'",
                rusqlite::params![1_800_000_000_000i64],
            )
            .unwrap();
        assert_eq!(
            definition_fingerprint(&temp.row("a1").unwrap()),
            definition_fingerprint(&temp.row("b1").unwrap()),
            "状态/运行时间/归属变化不改变任务身份"
        );

        temp.conn()
            .execute("UPDATE automations SET rrule = 'FREQ=HOURLY' WHERE id = 'b1'", [])
            .unwrap();
        assert_ne!(
            definition_fingerprint(&temp.row("a1").unwrap()),
            definition_fingerprint(&temp.row("b1").unwrap()),
            "真正改变调度定义必须改变 definitionKey"
        );
    }

    #[test]
    fn selection_parsing_rejects_bad_shapes() {
        let ok = json!([
            {"id": "a", "action": "copy"},
            {"id": "b", "action": "transfer"},
            {"id": "a", "action": "transfer"}
        ]);
        let parsed = parse_automation_selections(Some(&ok)).unwrap();
        assert_eq!(parsed.len(), 2, "重复 id 只保留第一次");
        assert_eq!(parsed[0].action, AutomationAction::Copy);
        assert_eq!(parsed[1].action, AutomationAction::Transfer);

        assert!(parse_automation_selections(None).unwrap().is_empty());
        assert!(parse_automation_selections(Some(&Value::Null))
            .unwrap()
            .is_empty());
        assert!(parse_automation_selections(Some(&json!({"id": "a"}))).is_err());
        assert!(parse_automation_selections(Some(&json!([{"id": "", "action": "copy"}]))).is_err());
        assert!(
            parse_automation_selections(Some(&json!([{"id": "a", "action": "move"}]))).is_err()
        );
    }

    #[test]
    fn first_cwd_reads_json_array() {
        assert_eq!(
            first_cwd(Some("[\"/tmp/a\", \"/tmp/b\"]")).as_deref(),
            Some("/tmp/a")
        );
        assert_eq!(first_cwd(Some("[]")), None);
        assert_eq!(first_cwd(Some("")), None);
        assert_eq!(first_cwd(None), None);
        assert_eq!(first_cwd(Some("not-json")), None);
    }

    #[test]
    fn deleted_flag_follows_client_semantics() {
        let mut row: RowPairs = Vec::new();
        row.push(("deleted_at".to_string(), SqlValue::Null));
        assert!(!is_deleted(&row));
        row[0].1 = SqlValue::Integer(-1);
        assert!(!is_deleted(&row));
        row[0].1 = SqlValue::Integer(1_700_000_000_000);
        assert!(is_deleted(&row));
    }

    // -----------------------------------------------------------------------
    // 写库集成测试：临时库 + 真 SQL，覆盖复制 / 转移 / 幂等 / 空间 / 越权
    // -----------------------------------------------------------------------

    use std::path::Path;

    struct TempDb {
        dir: PathBuf,
        db: PathBuf,
    }

    impl TempDb {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_automation_{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("workbuddy.db");
            let conn = Connection::open(&db).unwrap();
            // 只取本模块用到的列：与应用真实表同名列，动态列读取才不会漏字段。
            conn.execute_batch(
                "CREATE TABLE automations (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, prompt TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL, schedule_type TEXT NOT NULL DEFAULT 'recurring',
                    next_run_at INTEGER, last_run_at INTEGER, cwds TEXT NOT NULL DEFAULT '[]',
                    rrule TEXT NOT NULL DEFAULT '', scheduled_at TEXT, valid_from TEXT,
                    valid_until TEXT, model_id TEXT, model_is_thinking INTEGER NOT NULL DEFAULT 0,
                    push_to_wechat INTEGER NOT NULL DEFAULT 0,
                    push_to_wecom_bot INTEGER NOT NULL DEFAULT 0, wecom_bot_source TEXT,
                    wecom_bot_chat_id TEXT, created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL, deleted_at INTEGER,
                    skills_json TEXT NOT NULL DEFAULT '[]', owner_user_id TEXT,
                    owner_status TEXT NOT NULL DEFAULT 'legacy_unassigned', owner_source TEXT,
                    workspace_scope TEXT, space_id TEXT
                );
                CREATE TABLE spaces (
                    id TEXT PRIMARY KEY, user_id TEXT NOT NULL, device_id TEXT NOT NULL DEFAULT '',
                    name TEXT NOT NULL, primary_folder_path TEXT, deleted_at INTEGER NOT NULL DEFAULT -1,
                    status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL
                );",
            )
            .unwrap();
            Self { dir, db }
        }

        fn conn(&self) -> Connection {
            Connection::open(&self.db).unwrap()
        }

        fn insert_space(&self, id: &str, uid: &str, name: &str, path: &str) {
            self.conn()
                .execute(
                    "INSERT INTO spaces (id, user_id, name, primary_folder_path, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id, uid, name, path, 1_700_000_000_000i64],
                )
                .unwrap();
        }

        #[allow(clippy::too_many_arguments)]
        fn insert(
            &self,
            id: &str,
            name: &str,
            owner: Option<&str>,
            owner_status: &str,
            scope: Option<&str>,
            space_id: Option<&str>,
            schedule_type: &str,
            scheduled_at: Option<&str>,
            deleted_at: Option<i64>,
        ) {
            self.conn()
                .execute(
                    "INSERT INTO automations (
                        id, name, prompt, status, schedule_type, next_run_at, last_run_at, cwds,
                        rrule, scheduled_at, created_at, updated_at, deleted_at, owner_user_id,
                        owner_status, owner_source, workspace_scope, space_id
                     ) VALUES (?1, ?2, '', 'ACTIVE', ?3, ?4, ?5, '[\"/tmp/ws\"]', 'FREQ=DAILY', ?6,
                               ?7, ?7, ?8, ?9, ?10, 'created', ?11, ?12)",
                    rusqlite::params![
                        id,
                        name,
                        schedule_type,
                        1_700_000_100_000i64,
                        1_600_000_000_000i64,
                        scheduled_at,
                        1_600_000_000_000i64,
                        deleted_at,
                        owner,
                        owner_status,
                        scope,
                        space_id
                    ],
                )
                .unwrap();
        }

        fn row(&self, id: &str) -> Option<RowPairs> {
            read_row(&self.conn(), id).unwrap()
        }

        fn count(&self) -> i64 {
            self.conn()
                .query_row("SELECT COUNT(*) FROM automations", [], |row| row.get(0))
                .unwrap()
        }

        fn store_root(&self) -> PathBuf {
            self.dir.join("store")
        }

        fn backup_root(&self) -> PathBuf {
            self.dir.join("backups")
        }

        fn apply(
            &self,
            source: &str,
            target: &str,
            selections: &[AutomationSelection],
        ) -> Result<Option<Value>, String> {
            apply_at(
                &self.db,
                &self.store_root(),
                &self.backup_root(),
                source,
                target,
                Some(TEST_WEBHOOK),
                selections,
            )
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn copy(id: &str) -> AutomationSelection {
        AutomationSelection {
            id: id.to_string(),
            action: AutomationAction::Copy,
        }
    }

    fn transfer(id: &str) -> AutomationSelection {
        AutomationSelection {
            id: id.to_string(),
            action: AutomationAction::Transfer,
        }
    }

    /// 复制：源账号那一份原样保留，新行归属目标账号并按时可执行。
    #[test]
    fn copy_keeps_source_and_creates_owned_copy() {
        let temp = TempDb::new();
        temp.insert("a1", "每天领积分", Some("u1"), "confirmed", None, None, "recurring", None, None);

        let report = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
        let target_id = report["copied"][0]["targetId"].as_str().unwrap().to_string();
        assert_ne!(target_id, "a1");
        assert_eq!(report["copied"][0]["name"], "每天领积分");
        assert!(report["backup"].as_str().unwrap().contains("automations-"));

        let source = temp.row("a1").expect("源任务必须保留");
        assert_eq!(text_of(&source, "owner_user_id"), Some("u1"));
        assert_eq!(int_of(&source, "last_run_at"), Some(1_600_000_000_000));

        let copy_row = temp.row(&target_id).expect("必须写入副本");
        assert_eq!(text_of(&copy_row, "owner_user_id"), Some("u2"));
        assert_eq!(text_of(&copy_row, "owner_status"), Some(OWNER_STATUS_CONFIRMED));
        assert_eq!(text_of(&copy_row, "owner_source"), Some(OWNER_SOURCE_COPY));
        // 保留绝对计划时间：客户端据此判断是否已错过并重算下一次。
        assert_eq!(int_of(&copy_row, "next_run_at"), Some(1_700_000_100_000));
        assert_eq!(int_of(&copy_row, "last_run_at"), None);
        assert!(!is_deleted(&copy_row));
        assert_eq!(temp.count(), 2);

        // 复制关系落盘：下次切号据此去重。
        let links = load_links_at(&temp.store_root());
        assert_eq!(links.links.len(), 1);
        assert_eq!(links.links[0].source_id, "a1");
        assert_eq!(links.links[0].target_uid, "u2");
        assert_eq!(links.links[0].target_id, target_id);
    }

    /// 同一来源复制给同一目标只产生一份：第二次切号返回「已复制过」。
    #[test]
    fn copy_is_idempotent_per_target() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);

        let first = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        let target_id = first["copied"][0]["targetId"].as_str().unwrap().to_string();

        let second = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert!(second["copied"].as_array().unwrap().is_empty());
        assert_eq!(second["skipped"][0]["sourceId"], "a1");
        assert!(second["skipped"][0]["message"]
            .as_str()
            .unwrap()
            .contains("已复制过"));
        assert_eq!(temp.count(), 2);

        // 副本被删掉后可以重新复制（关系记录不能变成永久封锁）。
        temp.conn()
            .execute(
                "UPDATE automations SET deleted_at = ?1 WHERE id = ?2",
                rusqlite::params![1_700_000_000_000i64, target_id],
            )
            .unwrap();
        let third = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert_eq!(third["copied"].as_array().unwrap().len(), 1);
    }

    /// 往返切换：A→B→A→B 不再给任一账号多添一份（link 只认 sourceId，这里靠指纹兜底）。
    #[test]
    fn round_trip_copies_do_not_accumulate() {
        let temp = TempDb::new();
        temp.insert("a1", "课件草稿", Some("u1"), "confirmed", None, None, "recurring", None, None);

        let first = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert_eq!(first["copied"].as_array().unwrap().len(), 1);
        let b1 = first["copied"][0]["targetId"].as_str().unwrap().to_string();

        let back = temp.apply("u2", "u1", &[copy(&b1)]).unwrap().unwrap();
        assert!(back["copied"].as_array().unwrap().is_empty(), "回切不应再新增");
        assert!(back["skipped"][0]["message"]
            .as_str()
            .unwrap()
            .contains("已有相同任务"));
        assert_eq!(temp.count(), 2);

        let again = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert!(again["copied"].as_array().unwrap().is_empty());
        assert_eq!(temp.count(), 2, "每个账号始终只有一份");
    }

    /// 同一次切换里勾选了两条等价任务：只落一份副本。
    #[test]
    fn equivalent_tasks_in_one_batch_copy_only_once() {
        let temp = TempDb::new();
        temp.insert("a1", "同名任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.insert("a2", "同名任务", Some("u1"), "confirmed", None, None, "recurring", None, None);

        let report = temp
            .apply("u1", "u2", &[copy("a1"), copy("a2")])
            .unwrap()
            .unwrap();
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
        assert_eq!(report["skipped"].as_array().unwrap().len(), 1);
        assert_eq!(temp.count(), 3);
    }

    /// 目标账号已有同定义任务（状态/运行时间不同也算同一份）：跳过而不新增。
    #[test]
    fn copy_skips_equivalent_target_with_volatile_fields_changed() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.insert("b1", "任务", Some("u2"), "confirmed", None, None, "recurring", None, None);
        temp.conn()
            .execute(
                "UPDATE automations SET status = 'PAUSED', next_run_at = ?1, last_run_at = ?2 \
                 WHERE id = 'b1'",
                rusqlite::params![1_800_000_000_000i64, 1_500_000_000_000i64],
            )
            .unwrap();

        let report = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert!(report["copied"].as_array().unwrap().is_empty());
        assert!(report["skipped"][0]["message"]
            .as_str()
            .unwrap()
            .contains("已有相同任务"));
        assert_eq!(temp.count(), 2);
        assert_eq!(text_of(&temp.row("b1").unwrap(), "status"), Some("PAUSED"));
    }

    /// 定义不同（这里只是名字不同）不算重复，仍要正常复制。
    #[test]
    fn different_definition_is_not_treated_as_duplicate() {
        let temp = TempDb::new();
        temp.insert("a1", "任务A", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.insert("b1", "任务B", Some("u2"), "confirmed", None, None, "recurring", None, None);

        let report = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
        assert_eq!(temp.count(), 3);
    }

    /// 转移也不能在目标账号凑出两份一样的任务。
    #[test]
    fn transfer_skips_when_target_already_has_equivalent() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.insert("b1", "任务", Some("u2"), "confirmed", None, None, "recurring", None, None);

        let report = temp
            .apply("u1", "u2", &[transfer("a1")])
            .unwrap()
            .unwrap();
        assert!(report["transferred"].as_array().unwrap().is_empty());
        assert!(report["skipped"][0]["message"]
            .as_str()
            .unwrap()
            .contains("未转移"));
        assert_eq!(text_of(&temp.row("a1").unwrap(), "owner_user_id"), Some("u1"));
    }

    /// 转移：只改归属，id 与运行记录不动；源账号随之失去这条任务。
    #[test]
    fn transfer_only_changes_owner() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);

        let report = temp.apply("u1", "u2", &[transfer("a1")]).unwrap().unwrap();
        assert_eq!(report["transferred"][0]["sourceId"], "a1");
        assert_eq!(report["transferred"][0]["name"], "任务");
        assert!(report["copied"].as_array().unwrap().is_empty());
        assert_eq!(temp.count(), 1, "转移不产生第二行");

        let row = temp.row("a1").unwrap();
        assert_eq!(text_of(&row, "owner_user_id"), Some("u2"));
        assert_eq!(text_of(&row, "owner_status"), Some(OWNER_STATUS_CONFIRMED));
        assert_eq!(text_of(&row, "owner_source"), Some(OWNER_SOURCE_TRANSFER));
        assert_eq!(int_of(&row, "last_run_at"), Some(1_600_000_000_000));

        // 目标已经是自己：跳过而不是重复写。
        let again = temp.apply("u2", "u2", &[transfer("a1")]);
        assert!(again.is_err(), "源与目标相同必须直接拒绝");
    }

    /// 空间作用域：目标账号有同路径空间就重映射，没有就降级为 task 作用域。
    #[test]
    fn workspace_space_remaps_or_downgrades() {
        let temp = TempDb::new();
        temp.insert_space("sp-src", "u1", "监控", "/Users/me/WorkBuddy/monitor");
        // 目标账号的同路径空间（会被认出来重映射）。
        temp.insert_space("sp-dst", "u2", "监控", "/Users/me/WorkBuddy/monitor");
        // 另一条源空间路径在目标账号不存在（会被降级）。
        temp.insert_space("sp-src-2", "u1", "课件", "/Users/me/WorkBuddy/courseware");
        temp.insert(
            "a1",
            "有同名空间",
            Some("u1"),
            "confirmed",
            Some("workspace"),
            Some("sp-src"),
            "recurring",
            None,
            None,
        );
        temp.insert(
            "a2",
            "没有对应空间",
            Some("u1"),
            "confirmed",
            Some("workspace"),
            Some("sp-src-2"),
            "recurring",
            None,
            None,
        );

        let report = temp
            .apply("u1", "u2", &[copy("a1"), copy("a2")])
            .unwrap()
            .unwrap();
        // 两条都复制成功；降级的那条带一条 note。
        assert_eq!(report["copied"].as_array().unwrap().len(), 2);
        let notes = report["notes"].as_array().unwrap();
        assert!(notes
            .iter()
            .any(|note| note["message"].as_str().unwrap().contains("降级为任务作用域")));

        let copied: Vec<String> = report["copied"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["targetId"].as_str().unwrap().to_string())
            .collect();
        let first = temp.row(&copied[0]).unwrap();
        // a1（第一条）：目标账号有同路径空间 → 重映射。
        assert_eq!(text_of(&first, "space_id"), Some("sp-dst"));
        assert_eq!(text_of(&first, "workspace_scope"), Some("workspace"));

        // 目标账号换成 u3（没有空间）→ 降级。
        temp.insert(
            "a3",
            "u3 无空间",
            Some("u1"),
            "confirmed",
            Some("workspace"),
            Some("sp-src"),
            "recurring",
            None,
            None,
        );
        let report = temp.apply("u1", "u3", &[copy("a3")]).unwrap().unwrap();
        let target_id = report["copied"][0]["targetId"].as_str().unwrap().to_string();
        let row = temp.row(&target_id).unwrap();
        assert_eq!(text_of(&row, "space_id"), None);
        assert_eq!(text_of(&row, "workspace_scope"), Some("task"));
        assert_eq!(
            text_of(&row, "cwds"),
            Some("[\"/tmp/ws\"]"),
            "降级只改作用域，工作目录必须保留"
        );
    }

    /// 越权与已删除：都不写库，只进 errors。
    #[test]
    fn invisible_or_deleted_tasks_are_rejected() {
        let temp = TempDb::new();
        temp.insert("other", "别人的任务", Some("u9"), "confirmed", None, None, "recurring", None, None);
        temp.insert(
            "gone",
            "已删除",
            Some("u1"),
            "confirmed",
            None,
            None,
            "recurring",
            None,
            Some(1_700_000_000_000),
        );
        temp.insert("legacy", "未归属任务", None, "legacy_unassigned", None, None, "recurring", None, None);

        let report = temp
            .apply("u1", "u2", &[copy("other"), copy("gone"), copy("legacy")])
            .unwrap()
            .unwrap();
        assert_eq!(temp.count(), 4, "越权与已删除不写库；未归属任务可复制出一份");
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2);
        assert!(errors
            .iter()
            .any(|item| item["error"].as_str().unwrap().contains("归属其他账号")));
        assert!(errors
            .iter()
            .any(|item| item["error"].as_str().unwrap().contains("已删除")));
        // 未认领任务对所有账号可见，可以复制。
        assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn empty_or_same_account_short_circuits() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        assert!(temp.apply("u1", "u2", &[]).unwrap().is_none());
        assert!(temp.apply("u1", "u1", &[copy("a1")]).is_err());
        let report = temp.apply("", "u2", &[copy("a1")]).unwrap_err();
        assert!(report.contains("未检测到当前登录账号"));
    }

    #[test]
    fn once_task_with_past_schedule_carries_note() {
        let temp = TempDb::new();
        temp.insert(
            "once",
            "一次性",
            Some("u1"),
            "confirmed",
            None,
            None,
            "once",
            Some("2020-01-01T09:00"),
            None,
        );
        let report = temp.apply("u1", "u2", &[copy("once")]).unwrap().unwrap();
        assert!(report["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["message"].as_str().unwrap().contains("不会执行")));
    }

    /// 带通知开关的任务：结果里必须提醒「渠道绑定不会随任务走」。
    #[test]
    fn notification_tasks_carry_webhook_in_prompt() {
        let temp = TempDb::new();
        temp.insert("a1", "带通知", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.conn()
            .execute("UPDATE automations SET push_to_wecom_bot = 1 WHERE id = 'a1'", [])
            .unwrap();

        let report = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert!(report["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["message"].as_str().unwrap().contains("通知随任务走")));
        let target_id = report["copied"][0]["targetId"].as_str().unwrap();
        let copy_row = temp.row(target_id).unwrap();
        let prompt = text_of(&copy_row, "prompt").unwrap();
        assert!(prompt.contains(WECOM_NOTIFY_START), "通知块写进 prompt");
        assert!(prompt.contains(TEST_WEBHOOK), "Webhook 随任务走");
        assert_eq!(int_of(&copy_row, "push_to_wecom_bot"), Some(1));

        // 注入通知块后仍要与源任务视为同一份，否则往返切换会再次新增副本。
        assert_eq!(
            definition_fingerprint(&temp.row("a1").unwrap()),
            definition_fingerprint(&copy_row),
        );
        let back = temp.apply("u2", "u1", &[copy(target_id)]).unwrap().unwrap();
        assert!(back["copied"].as_array().unwrap().is_empty());
        assert_eq!(temp.count(), 2);
    }

    /// 老副本（切号前生成，prompt 没有通知块）再次勾选时就地补写，不新增任务行。
    #[test]
    fn existing_copy_gets_webhook_upgrade_without_new_row() {
        let temp = TempDb::new();
        temp.insert("a1", "带通知", Some("u1"), "confirmed", None, None, "recurring", None, None);
        temp.conn()
            .execute("UPDATE automations SET push_to_wecom_bot = 1 WHERE id = 'a1'", [])
            .unwrap();
        let first = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        let target_id = first["copied"][0]["targetId"].as_str().unwrap().to_string();
        temp.conn()
            .execute(
                "UPDATE automations SET prompt = '老任务正文' WHERE id = ?1",
                rusqlite::params![target_id],
            )
            .unwrap();

        let again = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        assert!(again["copied"].as_array().unwrap().is_empty());
        assert!(again["skipped"][0]["message"]
            .as_str()
            .unwrap()
            .contains("已补写"));
        let upgraded_row = temp.row(&target_id).unwrap();
        let prompt = text_of(&upgraded_row, "prompt").unwrap();
        assert!(prompt.contains("老任务正文") && prompt.contains(WECOM_NOTIFY_START));
        assert_eq!(temp.count(), 2);
    }

    /// 备份必须真的生成文件：写库前留一份可回滚的整库副本。
    #[test]
    fn backup_is_written_before_mutation() {
        let temp = TempDb::new();
        temp.insert("a1", "任务", Some("u1"), "confirmed", None, None, "recurring", None, None);
        let report = temp.apply("u1", "u2", &[copy("a1")]).unwrap().unwrap();
        let backup = report["backup"].as_str().unwrap();
        assert!(Path::new(backup).join("workbuddy.db").is_file());
    }
}
