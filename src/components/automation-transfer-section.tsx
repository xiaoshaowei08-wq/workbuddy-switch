import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2 } from "lucide-react";

import * as api from "@/lib/api";
import { accountVariant } from "@/lib/variant";
import type {
  AccountMeta,
  AutomationAction,
  AutomationItem,
  AutomationSelection,
} from "@/lib/types";

/**
 * 切号弹窗的「定时任务」区块。
 *
 * 任务不是会话：可见性与到点派发都由 `automations.owner_user_id` 决定，切号只换 auth
 * 文件不会带走它们，旧账号的任务在新账号下既看不到也不会执行。这里让用户挑任务并选
 * 处理方式：
 *   - 复制：新 id、新归属，源账号那一份保留；
 *   - 转移：改归属，任务跟着账号走，源账号失去。
 *
 * 默认勾选全部 ACTIVE（复制）：切完号该跑的就都在跑，暂停中的仍需手动挑。
 */

export interface AutomationTransferState {
  selections: AutomationSelection[];
  total: number;
  active: number;
  loading: boolean;
  error: string;
}

interface Props {
  open: boolean;
  account: AccountMeta | null;
  disabled?: boolean;
  onChange: (state: AutomationTransferState) => void;
}

const LIST_MIN_H = "min-h-[min(7.5rem,26vh)]";

/** RRULE → 人话。只覆盖客户端支持的几种频率，认不出就原样显示。 */
export function describeSchedule(item: AutomationItem): string {
  if (item.scheduleType === "once") {
    return item.scheduledAt ? `一次性 · ${item.scheduledAt.replace("T", " ")}` : "一次性";
  }
  const rrule = item.rrule?.trim();
  if (!rrule) return "未设置周期";
  const parts = new Map<string, string>();
  for (const chunk of rrule.split(";")) {
    const [key, value] = chunk.split("=");
    if (key && value) parts.set(key.trim().toUpperCase(), value.trim());
  }
  const hour = parts.get("BYHOUR");
  const minute = parts.get("BYMINUTE") ?? "0";
  const time = hour ? `${hour.padStart(2, "0")}:${minute.padStart(2, "0")}` : "";
  const interval = Math.max(1, Number(parts.get("INTERVAL") ?? "1") || 1);
  switch ((parts.get("FREQ") ?? "").toUpperCase()) {
    case "HOURLY":
      return interval > 1 ? `每 ${interval} 小时` : "每小时";
    case "MINUTELY":
      return interval > 1 ? `每 ${interval} 分钟` : "每分钟";
    case "DAILY":
      return `${interval > 1 ? `每 ${interval} 天` : "每天"}${time ? ` ${time}` : ""}`.trim();
    case "WEEKLY": {
      const names: Record<string, string> = {
        MO: "一",
        TU: "二",
        WE: "三",
        TH: "四",
        FR: "五",
        SA: "六",
        SU: "日",
      };
      const days = (parts.get("BYDAY") ?? "")
        .split(",")
        .map((day) => names[day.trim().toUpperCase()] ?? "")
        .filter(Boolean)
        .join("、");
      return `${interval > 1 ? `每 ${interval} 周` : "每周"}${days ? `${days}` : ""}${
        time ? ` ${time}` : ""
      }`.trim();
    }
    case "MONTHLY":
      return `每月 ${parts.get("BYMONTHDAY") ?? "1"} 日${time ? ` ${time}` : ""}`;
    case "YEARLY":
      return `每年 ${parts.get("BYMONTH") ?? "1"} 月 ${parts.get("BYMONTHDAY") ?? "1"} 日`;
    default:
      return rrule;
  }
}

function statusText(status: string): string {
  if (status === "ACTIVE") return "启用中";
  if (status === "PAUSED") return "已暂停";
  return status || "未知";
}

export function AutomationTransferSection({ open, account, disabled, onChange }: Props) {
  const accountId = account?.id;
  const variant = accountVariant(account);
  const [items, setItems] = useState<AutomationItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  /** 勾选集合：值为该行选择的处理方式（不勾选则不存在）。 */
  const [selected, setSelected] = useState<Record<string, AutomationAction>>({});
  /** 目标账号已有的任务定义指纹：这些项默认不勾选，并显式标注。 */
  const [targetKeys, setTargetKeys] = useState<Set<string>>(new Set());
  const [targetCheckFailed, setTargetCheckFailed] = useState(false);
  /** 避免每次渲染都向上抛新对象：只在内容真的变化时回调。 */
  const lastEmitted = useRef("");
  const targetUid = account?.uid?.trim() || null;
  const alreadyInTarget = (item: AutomationItem) =>
    Boolean(item.definitionKey && targetKeys.has(item.definitionKey));
  /** 目标已有的任务默认不勾；但「待补通知」的老副本要勾上，切号时补写 Webhook。 */
  const shouldSelectByDefault = (item: AutomationItem) =>
    item.status === "ACTIVE" && (item.needsNotificationUpgrade || !alreadyInTarget(item));

  useEffect(() => {
    if (!open || !accountId) return;
    let cancelled = false;
    setItems([]);
    setSelected({});
    setTargetKeys(new Set());
    setTargetCheckFailed(false);
    setError("");
    setLoading(true);
    // 目标账号读取失败不挡住列表：保持原来的「勾选全部启用中」，落库时仍有服务端兜底。
    const current = api.listAutomations(variant);
    const target = targetUid
      ? api.listAutomations(variant, targetUid).catch(() => null)
      : Promise.resolve(null);
    void Promise.all([current, target])
      .then(([res, targetRes]) => {
        if (cancelled) return;
        setItems(res.automations);
        const keys = new Set(
          (targetRes?.automations ?? [])
            .map((item) => item.definitionKey)
            .filter((key) => Boolean(key)),
        );
        setTargetKeys(keys);
        setTargetCheckFailed(Boolean(targetUid) && targetRes === null);
        // 默认只勾目标账号没有的启用中任务；待补通知的老副本例外，要勾上触发补写。
        const defaults: Record<string, AutomationAction> = {};
        for (const item of res.automations) {
          if (
            item.status === "ACTIVE" &&
            (item.needsNotificationUpgrade ||
              !(item.definitionKey && keys.has(item.definitionKey)))
          ) {
            defaults[item.id] = "copy";
          }
        }
        setSelected(defaults);
      })
      .catch((e) => {
        if (!cancelled) setError(api.asError(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, accountId, variant, targetUid]);

  const active = useMemo(
    () => items.filter((item) => item.status === "ACTIVE").length,
    [items],
  );
  const alreadyInTargetCount = useMemo(
    () =>
      items.filter((item) =>
        Boolean(item.definitionKey && targetKeys.has(item.definitionKey)),
      ).length,
    [items, targetKeys],
  );
  const selectableActive = useMemo(
    () =>
      items.filter(
        (item) =>
          item.status === "ACTIVE" &&
          (item.needsNotificationUpgrade ||
            !(item.definitionKey && targetKeys.has(item.definitionKey))),
      ).length,
    [items, targetKeys],
  );

  const selections = useMemo<AutomationSelection[]>(
    () =>
      items
        .filter((item) => selected[item.id])
        .map((item) => ({ id: item.id, action: selected[item.id] })),
    [items, selected],
  );

  useEffect(() => {
    const state: AutomationTransferState = {
      selections,
      total: items.length,
      active,
      loading,
      error,
    };
    const signature = JSON.stringify({
      ids: selections.map((item) => `${item.id}:${item.action}`),
      total: items.length,
      active,
      loading,
      error,
    });
    if (signature === lastEmitted.current) return;
    lastEmitted.current = signature;
    onChange(state);
  }, [selections, items.length, active, loading, error, onChange]);

  function toggle(id: string) {
    setSelected((prev) => {
      const next = { ...prev };
      if (next[id]) delete next[id];
      else next[id] = "copy";
      return next;
    });
  }

  function setAction(id: string, action: AutomationAction) {
    setSelected((prev) => ({ ...prev, [id]: action }));
  }

  function selectAllActive() {
    const next: Record<string, AutomationAction> = {};
    for (const item of items) {
      if (shouldSelectByDefault(item)) next[item.id] = "copy";
    }
    setSelected(next);
  }

  const selectedCount = selections.length;
  const allActiveSelected = selectableActive > 0 && selectedCount === selectableActive;

  return (
    <div className="space-y-2">
      {loading ? (
        <p className="px-1 text-xs text-muted-foreground">正在加载定时任务…</p>
      ) : error ? (
        <p className="px-1 text-xs text-amber-700 dark:text-amber-400">
          无法加载定时任务：{error}
        </p>
      ) : items.length === 0 ? (
        <p className="px-1 text-xs text-muted-foreground">当前账号没有定时任务</p>
      ) : (
        <div className="flex items-center justify-between gap-2 px-1">
          <p className="min-w-0 text-xs text-muted-foreground">
            共 {items.length} 个 · {active} 个启用中
            {alreadyInTargetCount > 0 ? ` · ${alreadyInTargetCount} 个目标账号已有` : ""}
            ；任务归属决定谁能看到并执行
          </p>
          <button
            type="button"
            className="shrink-0 text-xs text-primary hover:underline disabled:opacity-50"
            onClick={() => (allActiveSelected ? setSelected({}) : selectAllActive())}
            disabled={disabled || (selectableActive === 0 && selectedCount === 0)}
          >
            {allActiveSelected ? "清空" : "全选启用中"}
          </button>
        </div>
      )}

      {targetCheckFailed && !loading && (
        <p className="px-1 text-xs text-amber-700 dark:text-amber-400">
          无法读取目标账号的现有任务，已按全部启用中任务勾选；实际处理时仍会按任务定义去重。
        </p>
      )}

      {loading ? (
        <div
          className={`flex items-center justify-center gap-2 text-sm text-muted-foreground ${LIST_MIN_H}`}
        >
          <Loader2 className="animate-spin" /> 加载任务…
        </div>
      ) : items.length === 0 ? (
        <p
          className={`flex items-center justify-center px-3 text-center text-sm text-muted-foreground ${LIST_MIN_H}`}
        >
          {error ? "无法加载定时任务列表" : "当前账号没有定时任务"}
        </p>
      ) : (
        <div
          className={`max-h-[min(22rem,45vh)] space-y-0.5 overflow-y-auto pr-1 ${LIST_MIN_H}`}
        >
          {items.map((item) => {
            const action = selected[item.id];
            return (
              <div
                key={item.id}
                className="flex items-start gap-2.5 rounded-md py-1.5 pr-1 hover:bg-accent/50"
              >
                <input
                  type="checkbox"
                  className="mt-0.5 size-3.5 shrink-0 cursor-pointer accent-primary"
                  checked={Boolean(action)}
                  onChange={() => toggle(item.id)}
                  disabled={disabled}
                  aria-label={`选择任务 ${item.name}`}
                />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm" title={item.name}>
                    {item.name}
                  </div>
                  <div className="truncate text-xs text-muted-foreground">
                    {describeSchedule(item)} · {statusText(item.status)}
                  </div>
                </div>
                {alreadyInTarget(item) && (
                  <span className="mt-0.5 shrink-0 rounded border border-amber-600/50 px-1 py-0.5 text-[10px] text-amber-700 dark:text-amber-400">
                    {item.needsNotificationUpgrade ? "待补通知" : "目标已有"}
                  </span>
                )}
                {action && (
                  <div className="mt-0.5 flex shrink-0 overflow-hidden rounded-md border text-[11px]">
                    {(["copy", "transfer"] as const).map((mode) => (
                      <button
                        key={mode}
                        type="button"
                        className={`px-1.5 py-0.5 ${
                          action === mode
                            ? "bg-primary text-primary-foreground"
                            : "text-muted-foreground hover:bg-accent"
                        }`}
                        onClick={() => setAction(item.id, mode)}
                        disabled={disabled}
                        title={
                          mode === "copy"
                            ? "复制：源账号保留这一份"
                            : "转移：任务跟着走，源账号失去"
                        }
                      >
                        {mode === "copy" ? "复制" : "转移"}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}

      <p className="px-1 text-xs text-muted-foreground">
        「复制」源账号保留，「转移」源账号失去；标记「目标已有」的不会默认勾选，手动勾选也会被服务端去重。「待补通知」的老副本会默认勾选，切号时补写企业微信群机器人 Webhook，不新增任务。
      </p>
    </div>
  );
}
