import { useEffect, useRef, useState } from "react";
import { CircleCheck, Loader2, PlaneTakeoff, RefreshCw } from "lucide-react";
import { toast } from "sonner";

import { DemoAction } from "@/components/demo-action";
import {
  AccordionSettingsRow,
  INSET_PANEL,
  NumberSettingRow,
  PANEL_ROW,
  SettingsFieldRow,
  formatTime,
  numberInputHint,
  resolveNumberInput,
  type NumberFieldSpec,
  type SaveCommit,
} from "@/components/settings-primitives";
import { Accordion } from "@/components/ui/accordion";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { TimePicker } from "@/components/ui/time-picker";
import { displayName } from "@/lib/account-display";
import * as api from "@/lib/api";
import type {
  AccountMeta,
  CheckinConfig,
  CheckinLog,
  TravelAccountProgress,
  TravelConfig,
  TravelOverview,
  TravelStatusLabel,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { accountVariant, variantSupportsCheckin, variantSupportsTravel } from "@/lib/variant";
import { useAccountsStore } from "@/stores/accounts";

function logLabel(result: string): { text: string; tone: "success" | "warning" | "error" } {
  switch (result) {
    case "success":
      return { text: "签到成功", tone: "success" };
    case "already":
      return { text: "已签到", tone: "warning" };
    default:
      return { text: "失败", tone: "error" };
  }
}

/** "HH:MM" → 当日分钟数；非法返回 null。 */
function clockMinutes(value: string): number | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value);
  if (!match) return null;
  const hour = Number(match[1]);
  const minute = Number(match[2]);
  if (hour > 23 || minute > 59) return null;
  return hour * 60 + minute;
}

/**
 * 签到时间段的非法组合说明；返回 null 表示可以落盘（两端都合法或两端都空）。
 *
 * 半填 / 非法组合只改本地显示、不写盘：后端把这种窗口当「不限制」处理，写盘会让下一轮
 * 对全部账号发起窗口外签到，所以文案要说明「未保存」。
 */
function checkinWindowIssue(start: string, end: string): string | null {
  if (!start && !end) return null;
  if (!start || !end) return "开始与结束时间需同时选择，当前选择未保存";
  const startMinutes = clockMinutes(start);
  const endMinutes = clockMinutes(end);
  if (startMinutes === null || endMinutes === null) return "时间格式应为 HH:MM，当前选择未保存";
  if (startMinutes >= endMinutes) return "结束时间需晚于开始时间（不支持跨午夜），当前选择未保存";
  return null;
}

/** 签到数字参数（后端不做范围校验，前端按这里的边界收敛后提交）。 */
const CHECKIN_NUMBER_FIELDS = {
  keepalive_days: { label: "保活阈值", min: 0, max: 90 },
  lazy_refresh_hours: { label: "惰性刷新", min: 1, max: 72 },
} as const satisfies Record<string, NumberFieldSpec>;

type CheckinNumberKey = keyof typeof CHECKIN_NUMBER_FIELDS;

/** 自动签到配置 + 一键签到 + 日志（含自动旅行行）。 */
function AutoCheckinCard() {
  /** 自动旅行与自动签到同卡，按档位决定是否渲染该行（国际版无成长中心）。 */
  const variant = useAccountsStore((s) => s.variant);
  const accounts = useAccountsStore((s) => s.accounts);
  const fetchAll = useAccountsStore((s) => s.fetchAll);
  const [cfg, setCfg] = useState<CheckinConfig | null>(null);
  /** 显示草稿的同步镜像：事件回调与异步回读都要读最新值，state 只负责渲染。 */
  const draftRef = useRef<CheckinConfig | null>(null);
  /** 最近一次落盘的配置：即时落盘以它为提交基准，未落盘的草稿不写盘。 */
  const savedRef = useRef<CheckinConfig | null>(null);
  /** 待提交编辑：同一次交互里的连续触发合并为一份最新快照。 */
  const pendingRef = useRef<SaveCommit<CheckinConfig> | null>(null);
  /** 提交链：串行发送，避免先发的那份（不含后一次编辑）后到达覆盖新值。 */
  const chainRef = useRef<Promise<void>>(Promise.resolve());
  /** 数字输入框草稿文本：只覆盖正在编辑的字段，失焦提交后清空。 */
  const [numDraft, setNumDraft] = useState<Partial<Record<CheckinNumberKey, string>>>({});
  /**
   * 手风琴展开的面板：参数区 / 逐账号开关 / 签到日志。
   *
   * 用 multiple 而非 single：三块内容彼此独立，用户可能同时对照参数与账号名单。
   */
  const [openSections, setOpenSections] = useState<string[]>([]);
  const logsOpen = openSections.includes("logs");
  const [logs, setLogs] = useState<CheckinLog[] | null>(null);
  const [logsError, setLogsError] = useState("");
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void loadConfig();
  }, []);

  // 账号列表来自全局 store：为空时补拉一次（只取状态与账号，不拉积分）。
  useEffect(() => {
    if (accounts.length === 0) void fetchAll();
  }, [accounts.length, fetchAll]);

  // 日志懒加载：展开时才请求，每次展开重新拉取；收起态无可见列表，不请求。
  useEffect(() => {
    if (!logsOpen) return;
    let cancelled = false;
    void loadLogs(() => cancelled);
    return () => {
      cancelled = true;
    };
  }, [logsOpen]);

  async function loadConfig() {
    try {
      const loaded = await api.getAutoCheckinConfig();
      savedRef.current = loaded;
      adoptSaved(loaded);
    } catch (e) {
      toast.error("自动签到配置加载失败", { description: api.asError(e) });
    }
  }

  /** 更新显示草稿（state 供渲染，ref 供事件回调与异步回读读最新值）。 */
  function updateCfg(next: CheckinConfig) {
    draftRef.current = next;
    setCfg(next);
  }

  /** 落盘值回填显示：保留半填 / 非法时间段草稿，那是刻意不落盘的本地状态。 */
  function adoptSaved(next: CheckinConfig) {
    const draft = draftRef.current;
    if (draft && checkinWindowIssue(draft.checkin_start, draft.checkin_end) !== null) {
      updateCfg({ ...next, checkin_start: draft.checkin_start, checkin_end: draft.checkin_end });
      return;
    }
    updateCfg(next);
  }

  /**
   * 即时落盘：编辑先合并成一份最新快照，再串行提交。
   *
   * 同一次交互里开关 click 与输入框 blur 会先后触发，各自都带整份配置；并发发送时
   * 先发的那份（不含后一次编辑）可能后到达，把新值覆盖回去。
   */
  function enqueueCheckinSave(
    edits: Partial<CheckinConfig>,
    success?: SaveCommit<CheckinConfig>["success"],
  ) {
    if (!savedRef.current) return;
    const pending = pendingRef.current;
    pendingRef.current = { edits: { ...pending?.edits, ...edits }, success: success ?? pending?.success };
    setSaving(true);
    chainRef.current = chainRef.current.then(flushCheckinSave);
  }

  async function flushCheckinSave() {
    const commit = pendingRef.current;
    pendingRef.current = null;
    const saved = savedRef.current;
    if (!commit || !saved) return;
    try {
      const next = await api.saveAutoCheckinConfig({ ...saved, ...commit.edits });
      savedRef.current = next;
      // 回读值只在没有更新编辑排队时才整体覆盖显示，避免顶掉刚做出的改动。
      if (!pendingRef.current) adoptSaved(next);
      if (commit.success) {
        toast.success(commit.success.title, { description: commit.success.description });
      }
    } catch (e) {
      adoptSaved(saved);
      toast.error("自动签到设置保存失败", { description: api.asError(e) });
    } finally {
      if (!pendingRef.current) setSaving(false);
    }
  }

  /** 主开关：拨动即落盘，失败回滚到上一份已确认配置。 */
  function onToggleEnabled(enabled: boolean) {
    const current = draftRef.current;
    if (!current) return;
    updateCfg({ ...current, enabled });
    enqueueCheckinSave({ enabled }, { title: enabled ? "自动签到已开启" : "自动签到已关闭" });
  }

  /** 数字参数：失焦 / 回车提交；空值回退原值，越界收敛，与落盘值相同则不发请求。 */
  function onNumberCommit(key: CheckinNumberKey, raw: string) {
    const current = draftRef.current;
    const saved = savedRef.current;
    if (!current || !saved) return;
    const field = CHECKIN_NUMBER_FIELDS[key];
    const value = resolveNumberInput(raw, field);
    clearNumDraft(key);
    const next = { ...current };
    next[key] = value ?? saved[key];
    updateCfg(next);
    if (value === null) {
      toast.error(numberInputHint(field));
      return;
    }
    if (value === saved[key]) return;
    const edits: Partial<CheckinConfig> = {};
    edits[key] = value;
    enqueueCheckinSave(edits, { title: `${field.label}已保存` });
  }

  /**
   * 时间段：先只改本地显示；仅「两端都填且 start < end」或「两端都清空」才落盘。
   * 半填 / 非法组合不写盘（后端把这种窗口当「不限制」，写盘会让下一轮对全部账号发起签到）。
   */
  function onWindowChange(key: "checkin_start" | "checkin_end", value: string) {
    const current = draftRef.current;
    const saved = savedRef.current;
    if (!current || !saved) return;
    const next = { ...current };
    next[key] = value;
    updateCfg(next);
    if (checkinWindowIssue(next.checkin_start, next.checkin_end) !== null) return;
    if (next.checkin_start === saved.checkin_start && next.checkin_end === saved.checkin_end) return;
    enqueueCheckinSave(
      { checkin_start: next.checkin_start, checkin_end: next.checkin_end },
      next.checkin_start
        ? { title: "签到时间段已保存", description: `${next.checkin_start} 至 ${next.checkin_end}` }
        : { title: "已清除签到时间段限制" },
    );
  }

  /** 「清除」按钮：两端清空属完整语义，立即落盘。 */
  function clearWindow() {
    const current = draftRef.current;
    const saved = savedRef.current;
    if (!current || !saved) return;
    updateCfg({ ...current, checkin_start: "", checkin_end: "" });
    if (!saved.checkin_start && !saved.checkin_end) return;
    enqueueCheckinSave(
      { checkin_start: "", checkin_end: "" },
      { title: "已清除签到时间段限制" },
    );
  }

  async function loadLogs(isCancelled?: () => boolean) {
    setLogsError("");
    try {
      const res = await api.getCheckinLogs();
      if (!isCancelled?.()) setLogs(res.logs);
    } catch (e) {
      if (isCancelled?.()) return;
      setLogs(null);
      setLogsError(api.asError(e));
    }
  }

  async function checkinAllNow() {
    if (busy || saving) return;
    setBusy(true);
    try {
      const res = await api.checkinAll();
      if (res.status === "skipped" && res.reason === "already_running") {
        toast.error("签到任务正在进行，请稍后再试");
        return;
      }
      const ok = res.accounts.filter((a) => a.result === "success").length;
      const already = res.accounts.filter((a) => a.result === "already").length;
      const err = res.accounts.filter((a) => a.result === "error").length;
      const skipped = res.accounts.filter(
        (a) => a.result === "skipped" && a.reason === "auto_checkin_disabled",
      ).length;
      const detail = res.accounts
        .filter((a) => a.result === "error")
        .map((a) => `${a.email}（${a.error}）`)
        .join("；");
      const text = `签到完成：成功 ${ok}，已签 ${already}，失败 ${err}，已忽略 ${skipped} 个关闭自动签到的账号${detail ? `。${detail}` : ""}`;
      const allFailed = res.accounts.length > 0 && err === res.accounts.length;
      const allSkippedOrUnavailable =
        res.accounts.length === 0 || (ok === 0 && already === 0 && err === 0);
      if (allFailed) toast.error(text);
      else if (allSkippedOrUnavailable) toast.info(text);
      else toast.success(text);
      void loadConfig();
      if (logsOpen) void loadLogs();
    } catch (e) {
      toast.error("签到失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  /**
   * 逐账号开关：切换即落盘，提交最新一份名单。
   *
   * 与参数编辑共用同一条提交链，名单从当前显示草稿派生（连续切换不会互相覆盖），
   * 失败由提交链统一回滚并提示。
   */
  function onAutoCheckinChange(account: AccountMeta, allowed: boolean) {
    const current = draftRef.current;
    if (!current) return;
    const excluded = new Set(current.excluded_account_ids ?? []);
    if (allowed) excluded.delete(account.id);
    else excluded.add(account.id);
    const next = [...excluded];
    updateCfg({ ...current, excluded_account_ids: next });
    enqueueCheckinSave({ excluded_account_ids: next });
  }

  /** 数字输入框：输入期间只改本地草稿文本，失焦 / 回车才提交。 */
  function onNumberChange(key: CheckinNumberKey, text: string) {
    setNumDraft((prev) => ({ ...prev, [key]: text }));
  }

  function clearNumDraft(key: CheckinNumberKey) {
    setNumDraft((prev) => {
      const next = { ...prev };
      delete next[key];
      return next;
    });
  }

  /** 逐账号开关只列出支持签到的档位（国际版签到接口未开放）。 */
  const checkinAccounts = accounts.filter((account) => variantSupportsCheckin(accountVariant(account)));
  const excludedIds = new Set(cfg?.excluded_account_ids ?? []);
  const windowIssue = cfg ? checkinWindowIssue(cfg.checkin_start, cfg.checkin_end) : null;

  return (
    <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none" aria-label="自动签到">
      <CardContent className="space-y-0 p-0">
        <Accordion type="multiple" value={openSections} onValueChange={setOpenSections}>
          {cfg ? (
            <>
              <AccordionSettingsRow
                value="params"
                label="启用自动签到"
                description="为允许自动签到的账号核验状态并补签；可在下方按账号关闭"
                actions={
                  <>
                    <DemoAction>
                      <Switch
                        aria-label="启用自动签到"
                        checked={cfg.enabled}
                        onCheckedChange={onToggleEnabled}
                      />
                    </DemoAction>
                    <DemoAction>
                      <Button size="sm" variant="outline" onClick={checkinAllNow} disabled={busy || saving}>
                        {busy ? <Loader2 className="animate-spin" /> : <CircleCheck />}全部立即签到
                      </Button>
                    </DemoAction>
                  </>
                }
              >
                <div className={INSET_PANEL}>
                  <SettingsFieldRow
                    className={PANEL_ROW}
                    label="签到时间段"
                    description={
                      <>
                        留空为不限制。设置后每天在窗口内随机时刻自动签到。
                        <span className="mt-0.5 block">需 App 在窗口附近运行才能按时执行。</span>
                      </>
                    }
                  >
                    <div className="flex min-w-0 w-full flex-col items-end gap-1 sm:w-auto">
                      <div className="flex min-w-0 w-full flex-wrap items-center justify-end gap-2 sm:w-auto">
                        <DemoAction className="min-w-0 flex-1 sm:flex-none">
                          <TimePicker
                            className="min-w-0 flex-1 sm:flex-none"
                            value={cfg.checkin_start}
                            hourLabel="签到开始时间（小时）"
                            minuteLabel="签到开始时间（分钟）"
                            onChange={(v) => onWindowChange("checkin_start", v)}
                          />
                        </DemoAction>
                        <span className="shrink-0 text-xs text-muted-foreground">至</span>
                        <DemoAction className="min-w-0 flex-1 sm:flex-none">
                          <TimePicker
                            className="min-w-0 flex-1 sm:flex-none"
                            value={cfg.checkin_end}
                            hourLabel="签到结束时间（小时）"
                            minuteLabel="签到结束时间（分钟）"
                            onChange={(v) => onWindowChange("checkin_end", v)}
                          />
                        </DemoAction>
                        {(cfg.checkin_start || cfg.checkin_end) && (
                          <DemoAction>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="shrink-0"
                              onClick={clearWindow}
                            >
                              清除
                            </Button>
                          </DemoAction>
                        )}
                      </div>
                      {windowIssue && (
                        <p className="text-xs leading-4 text-amber-600">{windowIssue}</p>
                      )}
                    </div>
                  </SettingsFieldRow>

                  <NumberSettingRow
                    className={PANEL_ROW}
                    id="ac-keep"
                    spec={CHECKIN_NUMBER_FIELDS.keepalive_days}
                    description="天；0 表示每天无条件刷新"
                    value={numDraft.keepalive_days ?? String(cfg.keepalive_days)}
                    onChange={(text) => onNumberChange("keepalive_days", text)}
                    onCommit={(raw) => onNumberCommit("keepalive_days", raw)}
                  />
                  <NumberSettingRow
                    className={cn(PANEL_ROW, "border-b-0")}
                    id="ac-lazy"
                    spec={CHECKIN_NUMBER_FIELDS.lazy_refresh_hours}
                    description="小时"
                    value={numDraft.lazy_refresh_hours ?? String(cfg.lazy_refresh_hours)}
                    onChange={(text) => onNumberChange("lazy_refresh_hours", text)}
                    onCommit={(raw) => onNumberCommit("lazy_refresh_hours", raw)}
                  />
                </div>
              </AccordionSettingsRow>

              {/* 逐账号开关：切换即落盘，与参数编辑共用同一条提交链。 */}
              <AccordionSettingsRow
                value="excluded"
                label="不参与自动签到的账号"
                description="关闭后，后台轮次、刷新积分与全部立即签到都会跳过该账号；仍可在账号卡片进行单账号手动签到。"
              >
                <div className={INSET_PANEL}>
                  {checkinAccounts.length === 0 ? (
                    <p className="py-2 text-xs text-muted-foreground">暂无可签到的账号</p>
                  ) : (
                    checkinAccounts.map((account) => {
                      const name = displayName(account);
                      return (
                        <div
                          key={account.id}
                          className="flex min-w-0 items-center justify-between gap-3 border-b border-border/50 py-2.5 last:border-b-0"
                        >
                          <span className="min-w-0 flex-1 truncate text-xs leading-4" title={name}>
                            {name}
                          </span>
                          <DemoAction>
                            <Switch
                              checked={!excludedIds.has(account.id)}
                              onCheckedChange={(allowed) => onAutoCheckinChange(account, allowed)}
                              aria-label={`${name}参与自动签到`}
                            />
                          </DemoAction>
                        </div>
                      );
                    })
                  )}
                </div>
              </AccordionSettingsRow>
            </>
          ) : (
            <p className="px-4 py-3 text-sm text-muted-foreground sm:px-5">加载配置中…</p>
          )}

          {/* 成长中心（派猫猫旅行）仅国内版开放：国际版不渲染该行，也不请求其配置。
              行本身独立于签到配置的加载状态，签到配置读取失败也不影响开关。 */}
          {variantSupportsTravel(variant) ? <AutoTravelRow /> : null}

          <AccordionSettingsRow
            value="logs"
            label="签到日志"
            description="保留最近 30 天；本机明文保存，可能含账号昵称。"
            divider={false}
          >
            <div className={INSET_PANEL}>
              {logsError ? (
                <p className="py-2 text-xs text-destructive">{logsError}</p>
              ) : !logs ? (
                <p className="py-2 text-xs text-muted-foreground">正在读取…</p>
              ) : logs.length === 0 ? (
                <p className="py-2 text-xs text-muted-foreground">暂无签到记录</p>
              ) : (
                <div className="max-h-64 overflow-y-auto pr-1">
                  {[...logs].reverse().map((l, i) => {
                    const tone = logLabel(l.result);
                    return (
                      <div
                        key={i}
                        className="flex items-center justify-between border-b border-border/50 py-2 text-xs last:border-b-0"
                      >
                        <div className="min-w-0 flex-1 truncate">
                          <span className="font-medium">{l.email}</span>
                          {l.error && <span className="text-destructive">（{l.error}）</span>}
                        </div>
                        <div className="ml-2 flex shrink-0 items-center gap-2">
                          <span
                            className={
                              tone.tone === "error"
                                ? "text-destructive"
                                : tone.tone === "warning"
                                  ? "text-amber-600"
                                  : "text-emerald-600"
                            }
                          >
                            {tone.text}
                          </span>
                          <span className="text-muted-foreground">{formatTime(l.ts)}</span>
                        </div>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          </AccordionSettingsRow>
        </Accordion>
      </CardContent>
    </Card>
  );
}

/**
 * 自动旅行：单行开关，渲染在「自动签到」卡片内部（不独立成卡）。
 *
 * 成长中心仅国内版开放，调用方按档位决定是否渲染；开关切换即落盘（无「保存配置」按钮），
 * 失败回滚并提示。
 */
function AutoTravelRow() {
  const [cfg, setCfg] = useState<TravelConfig | null>(null);
  const [busy, setBusy] = useState(false);
  const [dispatching, setDispatching] = useState(false);
  const [refreshKey, setRefreshKey] = useState(0);
  /** 开关保存与手动派发共用一把「操作中」锁，避免同轮重复触发。 */
  const operating = busy || dispatching;

  useEffect(() => {
    let cancelled = false;
    void api
      .getAutoTravelConfig()
      .then((config) => {
        if (!cancelled) setCfg(config);
      })
      .catch((e) => {
        if (!cancelled) toast.error("自动旅行配置加载失败", { description: api.asError(e) });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function onToggle(enabled: boolean) {
    if (!cfg || operating) return;
    const previous = cfg;
    setCfg({ ...cfg, enabled });
    setBusy(true);
    try {
      setCfg(await api.saveAutoTravelConfig({ ...cfg, enabled }));
      setRefreshKey((key) => key + 1);
      if (enabled) {
        toast.success("自动旅行已开启", { description: "正在按官方状态派发或领取" });
      } else {
        toast.success("自动旅行已关闭");
      }
    } catch (e) {
      setCfg(previous);
      toast.error("自动旅行设置保存失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  /** 手动派发：不必等 30 分钟后台轮次，也无需把开关关掉再打开。 */
  async function onDispatchAll() {
    if (operating) return;
    setDispatching(true);
    try {
      const res = await api.dispatchTravelAll();
      if (res.status === "skipped" && res.reason === "already_running") {
        toast.error("派发任务正在进行，请稍后再试");
        return;
      }
      if (res.status === "no_accounts" || res.status === "skipped") {
        toast.info("没有可派发的国内版账号");
        return;
      }
      const list = res.accounts ?? [];
      const sent = list.filter((a) => a.result === "success" && !a.skip).length;
      const already = list.filter((a) => a.result === "success" && a.skip).length;
      const noBuddy = list.filter((a) => a.skip === "no-buddy").length;
      const failed = list.filter((a) => a.result === "error" && a.skip !== "no-buddy").length;
      const text = `派发完成：新派出 ${sent}，今日已派/已结束 ${already}，无 Buddy ${noBuddy}，失败 ${failed}`;
      if (list.length === 0 || (sent === 0 && already === 0)) toast.info(text);
      else if (failed > 0 && sent === 0) toast.error(text);
      else toast.success(text);
      setRefreshKey((key) => key + 1);
    } catch (e) {
      toast.error("派发失败", { description: api.asError(e) });
    } finally {
      setDispatching(false);
    }
  }

  return (
    <>
      <SettingsFieldRow
        label="启用自动旅行"
        description="开启后按官方状态自动派发或领取旅行奖励；切换后立即生效，也可点右侧按钮立即派发一轮"
        htmlFor="at-enabled"
      >
        <div className="flex items-center gap-2">
          <DemoAction>
            <Button
              size="sm"
              variant="outline"
              onClick={() => void onDispatchAll()}
              disabled={operating || !cfg}
              aria-label="全部立即派发"
            >
              {dispatching ? <Loader2 className="animate-spin" /> : <PlaneTakeoff />}
              全部立即派发
            </Button>
          </DemoAction>
          <DemoAction>
            <Switch
              id="at-enabled"
              checked={cfg?.enabled ?? false}
              disabled={operating || !cfg}
              onCheckedChange={(v) => void onToggle(v)}
              aria-label="启用自动旅行"
            />
          </DemoAction>
        </div>
      </SettingsFieldRow>
      <AutoTravelStatusPanel refreshKey={refreshKey} />
    </>
  );
}

const TRAVEL_LABEL_STYLE: Record<TravelStatusLabel, { text: string; className: string }> = {
  traveling: { text: "旅行中", className: "bg-sky-500/15 text-sky-600 dark:text-sky-300" },
  finished: { text: "已结束", className: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-300" },
  "no-buddy": { text: "无 Buddy", className: "bg-amber-500/15 text-amber-600 dark:text-amber-300" },
  untraveled: { text: "未旅行", className: "bg-foreground/[0.06] text-muted-foreground" },
};

function travelProgress(account: TravelAccountProgress, nowMs: number): number | null {
  if (account.label !== "traveling" || account.already || !account.departAt || !account.arriveAt) return null;
  const total = account.arriveAt - account.departAt;
  if (total <= 0) return null;
  return Math.min(100, Math.max(0, ((nowMs / 1000 - account.departAt) / total) * 100));
}

function travelRemaining(arriveAt: number | null, nowMs: number): string | null {
  if (!arriveAt) return null;
  const seconds = arriveAt * 1000 - nowMs;
  if (seconds <= 0) return "即将到达";
  const minutes = Math.ceil(seconds / 60_000);
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  if (hours > 0 && rest > 0) return `剩余 ${hours} 小时 ${rest} 分钟`;
  if (hours > 0) return `剩余 ${hours} 小时`;
  return `剩余 ${rest} 分钟`;
}

function travelClock(seconds: number): string {
  return new Date(seconds * 1000).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
}

/**
 * 自动旅行今日状态：只读本地缓存快照，进度条与倒计时由前端按秒实时计算。
 * 每 30 秒重拉一次（窗口隐藏时暂停），开关切换后由 refreshKey 立即重拉。
 */
function AutoTravelStatusPanel({ refreshKey }: { refreshKey: number }) {
  const [overview, setOverview] = useState<TravelOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let cancelled = false;
    const load = () => {
      void api
        .getTravelOverview()
        .then((next) => {
          if (cancelled) return;
          setOverview(next);
          setError(null);
        })
        .catch((e) => {
          if (!cancelled) setError(api.asError(e));
        });
    };
    load();
    const timer = window.setInterval(() => {
      if (document.visibilityState !== "hidden") load();
    }, 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [refreshKey]);

  const hasActive = overview?.accounts.some((a) => a.label === "traveling" && a.arriveAt) ?? false;
  useEffect(() => {
    if (!hasActive) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [hasActive]);

  async function onRefresh() {
    setBusy(true);
    try {
      setOverview(await api.getTravelOverview());
      setError(null);
      setNow(Date.now());
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  const summaryItems: { text: string; count: number }[] = overview
    ? [
        { text: "旅行中", count: overview.summary.traveling },
        { text: "已结束", count: overview.summary.finished },
        { text: "无 Buddy", count: overview.summary.noBuddy },
        { text: "未旅行", count: overview.summary.untraveled },
      ]
    : [];

  return (
    <div className="mx-4 mb-3 rounded-lg bg-foreground/[0.04] px-3 py-3 sm:mx-5">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-1.5 text-xs font-medium">
          <PlaneTakeoff className="size-3.5" />
          今日旅行状态
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[11px] text-muted-foreground/75">
            {overview?.lastUpdatedAt
              ? `上次同步 ${travelClock(overview.lastUpdatedAt / 1000)}`
              : "尚无记录"}
          </span>
          <Button
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-[11px]"
            disabled={busy}
            onClick={() => void onRefresh()}
          >
            <RefreshCw className={cn("mr-1 size-3", busy && "animate-spin")} />
            刷新
          </Button>
        </div>
      </div>

      {error ? (
        <p className="mt-2 text-xs text-destructive">状态读取失败：{error}</p>
      ) : !overview ? (
        <p className="mt-2 text-xs text-muted-foreground">正在读取…</p>
      ) : (
        <>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {summaryItems.map((item) => (
              <span key={item.text} className="rounded bg-foreground/[0.06] px-1.5 py-0.5 text-[11px] text-muted-foreground">
                {item.text} {item.count}
              </span>
            ))}
            <span className="rounded bg-foreground/[0.06] px-1.5 py-0.5 text-[11px] text-muted-foreground">
              {overview.date}
            </span>
          </div>

          <div className="mt-2 max-h-72 space-y-1.5 overflow-y-auto pr-0.5">
            {overview.accounts.length === 0 ? (
              <p className="py-2 text-xs text-muted-foreground">暂无支持成长中心的国内版账号</p>
            ) : (
              overview.accounts.map((account) => {
                const style = TRAVEL_LABEL_STYLE[account.label];
                const progress = travelProgress(account, now);
                const remaining = account.label === "traveling" ? travelRemaining(account.arriveAt, now) : null;
                return (
                  <div key={account.accountId} className="rounded-md border border-border/40 px-2.5 py-2">
                    <div className="flex items-center justify-between gap-2">
                      <span className="min-w-0 truncate text-xs font-medium">{account.email}</span>
                      <span className={cn("shrink-0 rounded px-1.5 py-0.5 text-[11px]", style.className)}>
                        {style.text}
                      </span>
                    </div>
                    {progress !== null && (
                      <div className="mt-1.5 h-1.5 overflow-hidden rounded-full bg-foreground/10">
                        <div
                          className="h-full rounded-full bg-sky-500 transition-[width] duration-500"
                          style={{ width: `${progress}%` }}
                        />
                      </div>
                    )}
                    <div className="mt-1 flex flex-wrap gap-x-2 text-[11px] text-muted-foreground/75">
                      {account.locationName && <span>{account.locationName}</span>}
                      {account.rewardCredit != null && <span>预计 +{account.rewardCredit} 积分</span>}
                      {remaining && <span>{remaining}</span>}
                      {account.label === "traveling" && account.arriveAt && (
                        <span>预计 {travelClock(account.arriveAt)} 到达</span>
                      )}
                      {account.label === "traveling" && account.already && (
                        <span>网页端派出，无出发时刻，仅显示倒计时</span>
                      )}
                      {account.label === "no-buddy" && (
                        <span>需先切到该账号，在成长中心完成并「收下」「领取一只 Buddy」任务奖励；Buddy 到账后下一轮自动派出</span>
                      )}
                      {account.label === "untraveled" && <span>等待派发</span>}
                      {account.label === "finished" && <span>{account.claimed ? "奖励已领取" : "行程已结束"}</span>}
                    </div>
                  </div>
                );
              })
            )}
          </div>

          <p className="mt-2 text-[11px] leading-4 text-muted-foreground/75">
            派发每 {overview.dispatchIntervalMinutes} 分钟 · 领取每 {overview.claimIntervalMinutes} 分钟；
            {overview.enabled
              ? overview.completed
                ? "今日已完成，无可重试项。"
                : "后台持续补派中。"
              : "自动派发已关闭，仅展示已有行程。"}
            进度按官方返回的到达时间实时计算。
          </p>
        </>
      )}
    </div>
  );
}


/**
 * 自动签到：从设置页独立出来的侧栏页面。
 *
 * 配置、逐账号开关、一键签到、自动旅行与签到日志保持原有交互；演示模式下
 * 读接口返回演示数据，写操作由 DemoAction 拦截。
 */
export default function AutoCheckinPage() {
  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-10 sm:mb-12">
        <h1 className="text-2xl font-semibold tracking-tight">自动签到</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          按账号核验状态并补签；自动旅行按官方状态派发与领取奖励。
        </p>
      </header>
      <AutoCheckinCard />
    </div>
  );
}
