import type { ReactElement, ReactNode } from "react";
import { ChevronDown } from "lucide-react";

import { DemoAction } from "@/components/demo-action";
import { AccordionContent, AccordionItem, AccordionTrigger } from "@/components/ui/accordion";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";

/**
 * 设置页与自动签到页共用的版式原语。
 *
 * 原定义在 SettingsPage.tsx：自动签到拆成独立侧栏页面后，两边都要用同一套
 * 卡片 / 缩进 / 折叠行，故集中到这里，避免复制出两份规则。
 */

interface SettingsGroupProps {
  id: string;
  title: string;
  children: ReactNode;
}

/**
 * 折叠面板的底部分隔线。
 *
 * 用伪元素而不是 border：面板自身无边距，border 会从卡片边缘拉到边缘，与其它行
 * （左右各缩进 mx-4 / sm:mx-5）的线对不齐。线要跟着整块内容走，标题与内容之间不留线。
 * 末行不传此项，但面板仍须保留 relative——行尾箭头是绝对定位，缺定位祖先会飘到页面上。
 */
const ACCORDION_DIVIDER =
  "after:absolute after:inset-x-4 after:bottom-0 after:h-px after:bg-border/50 sm:after:inset-x-5";

function SettingsGroup({ id, title, children }: SettingsGroupProps) {
  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby={id}>
      <div className="px-1">
        <h2 id={id} className="text-[13px] font-medium leading-5">
          {title}
        </h2>
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">{children}</Card>
    </section>
  );
}

function SettingsRow({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "mx-4 flex min-w-0 items-center justify-between gap-3 border-b border-border/50 px-0 py-2.5 sm:mx-5",
        className,
      )}
    >
      {children}
    </div>
  );
}

interface SettingsFieldRowProps {
  label: ReactNode;
  description?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
  className?: string;
  operational?: boolean;
}

function SettingsFieldRow({
  label,
  description,
  htmlFor,
  children,
  className,
  operational = false,
}: SettingsFieldRowProps) {
  return (
    <SettingsRow className={cn("flex-col items-stretch gap-2 sm:flex-row sm:items-center", className)}>
      <div className="min-w-0 flex-1">
        {htmlFor ? (
          <Label htmlFor={htmlFor} className="text-[13px] leading-4">
            {label}
          </Label>
        ) : (
          <div className="text-[13px] font-medium leading-4">{label}</div>
        )}
        {description && (
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{description}</p>
        )}
      </div>
      <div className="flex min-w-0 w-full shrink-0 justify-end sm:w-auto">
        {operational ? <DemoAction className="w-full sm:w-auto">{children as ReactElement}</DemoAction> : children}
      </div>
    </SettingsRow>
  );
}

/** 展开内容的内嵌卡片：与外层行同一左右缩进，内部各行自带分隔线。 */
const INSET_PANEL = "mx-4 rounded-lg bg-foreground/[0.04] px-3 py-1 sm:mx-5";
/** 放进内嵌卡片的参数行：去掉外层缩进，改由卡片统一提供。 */
const PANEL_ROW = "mx-0 sm:mx-0";

interface AccordionSettingsRowProps {
  value: string;
  label: ReactNode;
  description?: ReactNode;
  /** 行内常驻操作（开关、按钮等）；有值时箭头改画在行尾，避开操作区。 */
  actions?: ReactNode;
  /** 是否带底部分隔线；末行传 false。 */
  divider?: boolean;
  children: ReactNode;
}

/**
 * 手风琴设置行：标题可点展开，展开内容在其下方。
 *
 * 行内带 Switch / Button 时触发区只能覆盖标题（button 不能嵌套 button），
 * 此时关掉触发区自带箭头、绝对定位到行尾，避免它落在标题与控件之间。
 */
function AccordionSettingsRow({
  value,
  label,
  description,
  actions,
  divider = true,
  children,
}: AccordionSettingsRowProps) {
  return (
    <AccordionItem value={value} className={cn("relative", divider && ACCORDION_DIVIDER)}>
      <div
        className={cn(
          "relative mx-4 flex min-w-0 flex-col items-stretch justify-between gap-2 py-2.5 sm:mx-5 sm:flex-row sm:items-center",
          actions ? "sm:pr-7" : undefined,
        )}
      >
        <AccordionTrigger chevron={!actions} className="min-w-0 flex-1 gap-0 py-0">
          <div className="min-w-0 flex-1">
            <div className="text-[13px] font-medium leading-4">{label}</div>
            {description && (
              <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{description}</p>
            )}
          </div>
          {actions && (
            // -right-1 抵消 p-1：让图形右缘与触发区自带箭头落在同一条线上。
            <ChevronDown className="absolute -right-1 top-1/2 hidden size-4 -translate-y-1/2 box-content p-1 text-muted-foreground transition-transform duration-200 sm:block" />
          )}
        </AccordionTrigger>
        {actions && (
          <div className="flex min-w-0 w-full shrink-0 flex-wrap items-center justify-end gap-2 sm:w-auto">
            {actions}
          </div>
        )}
      </div>
      <AccordionContent className="pt-1.5 pb-4">{children}</AccordionContent>
    </AccordionItem>
  );
}

interface NumberSettingRowProps {
  id: string;
  spec: NumberFieldSpec;
  description: ReactNode;
  value: string;
  onChange: (text: string) => void;
  onCommit: (raw: string) => void;
  /** 作为折叠区最后一行时传 border-b-0，避免与容器底部分隔线叠成双线。 */
  className?: string;
}

/**
 * 数字参数行：输入期间只改本地草稿文本，失焦 / 回车才提交。
 *
 * 用文本草稿而不是直接写回配置数值，才能区分「清空」与「0」，让空值在失焦时回退原值。
 */
function NumberSettingRow({
  id,
  spec,
  description,
  value,
  onChange,
  onCommit,
  className,
}: NumberSettingRowProps) {
  return (
    <SettingsFieldRow
      className={className}
      label={spec.label}
      description={description}
      htmlFor={id}
      operational
    >
      <Input
        id={id}
        className="w-full sm:w-48"
        type="number"
        min={spec.min}
        max={spec.max}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onBlur={(e) => onCommit(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") e.currentTarget.blur();
        }}
      />
    </SettingsFieldRow>
  );
}

function formatTime(ts: number): string {
  try {
    return new Date(ts).toLocaleString("zh-CN", {
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    return String(ts);
  }
}

/** 数字参数：展示名 + 收敛边界（与输入框 min/max 同源）。 */
interface NumberFieldSpec {
  label: string;
  min: number;
  max?: number;
}

/** 数字输入收敛：空 / 非数字返回 null（调用方回退到已落盘值），越界收敛到边界。 */
function resolveNumberInput(raw: string, field: NumberFieldSpec): number | null {
  const text = raw.trim();
  if (!text) return null;
  const parsed = Number(text);
  if (!Number.isFinite(parsed)) return null;
  const rounded = Math.round(parsed);
  const max = field.max ?? Number.POSITIVE_INFINITY;
  return Math.min(Math.max(rounded, field.min), max);
}

/** 空值 / 非数字的回退提示。 */
function numberInputHint(field: NumberFieldSpec): string {
  return field.max === undefined
    ? `${field.label}需为不小于 ${field.min} 的数字，已恢复为原值`
    : `${field.label}需为 ${field.min}–${field.max} 之间的数字，已恢复为原值`;
}

/** 待提交的一次落盘：编辑字段 + 成功后提示（连续编辑合并时提示取最新一次）。 */
interface SaveCommit<T> {
  edits: Partial<T>;
  success?: { title: string; description?: string };
}

export {
  SettingsGroup,
  SettingsRow,
  SettingsFieldRow,
  AccordionSettingsRow,
  NumberSettingRow,
  INSET_PANEL,
  PANEL_ROW,
  formatTime,
  resolveNumberInput,
  numberInputHint,
};
export type { NumberFieldSpec, SaveCommit };
