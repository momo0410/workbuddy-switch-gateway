import { ArrowRight, Cat, Check, CircleCheck, Clock3, Coins, Copy, Ellipsis, Globe, Info, Loader2, PencilLine, PlaneTakeoff, RefreshCw, Save, Sparkles, Star, Trash2 } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DemoAction } from "@/components/demo-action";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { CodeBuddyCnIdeMark, CodeBuddyMark, WorkBuddyMark } from "@/components/product-marks";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";
import { demoModeEnabled } from "@/lib/demo-mode";
import type { AccountMeta, CreditExpiry, CreditResource, TravelStatus } from "@/lib/types";

const AVATAR_TONES = [
  "bg-emerald-100 text-emerald-800",
  "bg-violet-100 text-violet-800",
  "bg-sky-100 text-sky-800",
  "bg-amber-100 text-amber-800",
  "bg-rose-100 text-rose-800",
  "bg-teal-100 text-teal-800",
] as const;

function avatarTone(name: string) {
  let hash = 0;
  for (let i = 0; i < name.length; i += 1) hash = (hash * 31 + name.charCodeAt(i)) >>> 0;
  return AVATAR_TONES[hash % AVATAR_TONES.length];
}

function formatCredits(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(value);
}

function formatCreditExpiry(ts: number | null): string {
  if (!ts) return "长期有效";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "长期有效";
  return `${String(date.getMonth() + 1).padStart(2, "0")}/${String(date.getDate()).padStart(2, "0")} 到期`;
}

function formatFullDate(ts: number | null): string {
  if (!ts) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleDateString("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit" });
}

function formatCreditUpdatedAt(ts: number | undefined): string {
  if (!ts) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}

function expiryClass(expired: boolean, expiringSoon: boolean): string {
  if (expired) return "text-destructive";
  if (expiringSoon) return "text-orange-600";
  return "text-muted-foreground";
}

function creditResources(credit?: CreditExpiry): CreditResource[] {
  return (credit?.resources ?? [])
    .filter((resource) => resource.remaining > 0)
    .map((resource, index) => ({ resource, index }))
    .sort((left, right) => {
      const leftExpiry = left.resource.expireAt ?? Number.POSITIVE_INFINITY;
      const rightExpiry = right.resource.expireAt ?? Number.POSITIVE_INFINITY;
      return leftExpiry === rightExpiry ? left.index - right.index : leftExpiry - rightExpiry;
    })
    .map(({ resource }) => resource);
}

function accountIdentity(account: AccountMeta): string {
  if (account.email) {
    const [local, domain] = account.email.split("@");
    if (!domain) return account.email;
    return `${local.slice(0, 1)}${"*".repeat(Math.max(3, local.length - 1))}@${domain}`;
  }
  return account.uid ? `UID · ${account.uid}` : `ID · ${account.id}`;
}

/**
 * 账号详情弹窗的字段行：[标签, 值, 悬停说明]。
 *
 * 目的：回答「我授权进来的到底是哪个号」。此前卡片只显示昵称与 uid/邮箱，
 * 而实际数据里还有手机号（国服账号的真实线索，其 email 常为空）、原始域名、
 * 账号类型、创建时间等 —— 这些恰好是辨认账号的关键。
 *
 * 值缺失时返回空串，由调用方渲染成「—」，保证行高与字段顺序稳定
 * （不因某个字段缺失而跳行）。
 */
function accountDetailRows(account: AccountMeta): [string, string, string?][] {
  const fmt = (ts: number | null | undefined) =>
    typeof ts === "number" && ts > 0 ? new Date(ts).toLocaleString("zh-CN") : "";
  return [
    // 备注强制转字符串：后端理论上保证 note 为 string|null，但万一返回对象
    // （如 issue #36 的加密信封 `{ $wbEncrypted, envelope }`），直接渲染对象会触发
    // React #31 整页白屏。这里兜底成字符串，配合后端 coerce 双保险。
    ["备注", account.note ? String(account.note) : "", "你自己填的标签，用于区分这是谁的号"],
    ["昵称", account.nickname ?? ""],
    ["邮箱", account.email ?? "", "国际版账号通常靠它辨认"],
    ["手机号", account.phoneNumber ?? "", "国服账号的邮箱常为空，手机号是主要线索"],
    ["UID", account.uid ?? ""],
    ["账号 ID", account.id, "本地账号库的主键（与上游 UID 不同）"],
    ["所属区域", account.region ?? "", "由登录域名推导：国服 / 国际版"],
    ["登录域名", account.domain ?? "", "排查问题时的确切域名，比区域标签更具体"],
    ["账号类型", account.accountType === "personal" ? "个人版" : account.accountType === "enterprise" ? "企业版" : (account.accountType ?? "")],
    ["企业 / 组织", account.enterpriseName ?? ""],
    ["Token 到期", fmt(account.expiresAt)],
    ["Refresh 到期", fmt(account.refreshExpiresAt), "超过此时间需重新登录"],
    ["上次刷新", fmt(account.refreshedAt)],
    ["加入时间", fmt(account.createdAt)],
    // 仅在异常时出现，避免平时多一行无意义的「正常」。
    ...(account.needsRelogin
      ? ([["状态", `需重新登录${account.needsReloginReason ? `（${account.needsReloginReason}）` : ""}`]] as [string, string, string?][])
      : []),
  ];
}

const chipClass = "rounded-md px-1.5 py-0 text-[11px] font-medium";

function travelIconChip({
  label,
  tooltip,
  variant,
}: {
  label: string;
  tooltip: string;
  variant: "secondary" | "success";
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant={variant} className={cn(chipClass, "px-1")} aria-label={label}>
          <PlaneTakeoff className="size-3.5" />
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top">{tooltip}</TooltipContent>
    </Tooltip>
  );
}

function formatTravelRemaining(arriveAt: number | null | undefined): string | null {
  if (!arriveAt || arriveAt <= 0) return null;
  const arriveMs = arriveAt > 1e12 ? arriveAt : arriveAt * 1000;
  const remainingMs = arriveMs - Date.now();
  if (remainingMs <= 0) return "即将到达";
  const totalMinutes = Math.max(1, Math.ceil(remainingMs / 60_000));
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours > 0 && minutes > 0) return `剩余 ${hours} 小时 ${minutes} 分钟`;
  if (hours > 0) return `剩余 ${hours} 小时`;
  return `剩余 ${minutes} 分钟`;
}

function travelTooltip(status: TravelStatus): string {
  const place = status.locationName?.trim();
  const credit = status.rewardCredit;
  const points = credit != null ? `+${credit}` : null;
  const remaining = formatTravelRemaining(status.arriveAt);
  if (status.label === "traveling") {
    const parts = [place, points ? `预计 ${points}` : "旅行中", remaining].filter(Boolean);
    return parts.length > 0 ? parts.join(" · ") : "旅行中";
  }
  if (status.label === "finished") {
    if (place && points) return `${place} · ${points}`;
    if (place) return `${place} · 已结束`;
    if (points) return `已结束 · ${points}`;
    return "已结束";
  }
  // 领养相关状态：把**具体原因**说清楚，而不是只说"无 Buddy"让用户猜。
  if (status.label === "adopted") {
    return status.rewardCredit != null
      ? `已领养 Buddy，获得 ${status.rewardCredit} 分；今日尚未派出`
      : "已领养 Buddy；今日尚未派出";
  }
  if (status.label === "adopt-threshold") {
    return "已尝试领养，但上游要求先积累足够的对话轮次；攒够后可再次领养（约 +300 分）";
  }
  if (status.label === "no-buddy") {
    return "尚无 Buddy，且本次领养未成功（可稍后重试）";
  }
  return "未旅行";
}

/** 按旅行状态渲染标签：领养状态 / 无 Buddy / 未旅行 / 旅行中 / 已结束。 */
function travelChip(status: TravelStatus | undefined) {
  if (!status) return null;
  switch (status.label) {
    // 领养相关状态一律用**带文字**的标签（而非旅行状态的纯图标）：
    // 「有没有猫」「为什么领不了」是用户要主动处理的信息，藏在 tooltip 里
    // 等于没说 —— 用户会反复点领养却不知道为什么失败。
    case "adopted":
      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="success" className={chipClass} aria-label="已领养 Buddy">
              <Cat className="size-3.5" />
              已领养
            </Badge>
          </TooltipTrigger>
          <TooltipContent side="top">{travelTooltip(status)}</TooltipContent>
        </Tooltip>
      );
    case "adopt-threshold":
      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="secondary" className={cn(chipClass, "text-muted-foreground")} aria-label="待攒对话后可领养">
              <Cat className="size-3.5" />
              待攒对话
            </Badge>
          </TooltipTrigger>
          <TooltipContent side="top">{travelTooltip(status)}</TooltipContent>
        </Tooltip>
      );
    case "no-buddy":
      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="secondary" className={cn(chipClass, "text-muted-foreground")} aria-label="无 Buddy">
              <Cat className="size-3.5" />
              无 Buddy
            </Badge>
          </TooltipTrigger>
          <TooltipContent side="top">{travelTooltip(status)}</TooltipContent>
        </Tooltip>
      );
    case "traveling":
      return travelIconChip({ label: travelTooltip(status), tooltip: travelTooltip(status), variant: "secondary" });
    case "finished":
      return travelIconChip({ label: travelTooltip(status), tooltip: travelTooltip(status), variant: "success" });
    case "untraveled":
    default:
      return <Badge variant="secondary" className={cn(chipClass, "text-muted-foreground")}>未旅行</Badge>;
  }
}

/** 国际版（workbuddy.ai）账号标注；国服账号不显示，避免噪音。 */
function regionChip(account: AccountMeta) {
  if (account.regionKey !== "intl") return null;
  const label = account.region || "国际版";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge
          variant="outline"
          className={cn(chipClass, "gap-1 border-sky-500/30 bg-sky-500/10 text-sky-700")}
          aria-label={`${label}账号`}
        >
          <Globe className="size-3" />
          {label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top">
        国际版账号（{account.regionKey === "intl" ? "workbuddy.ai" : ""}）· 不参与自动签到与自动旅行
      </TooltipContent>
    </Tooltip>
  );
}

interface Props {
  account: AccountMeta;
  onDelete: (a: AccountMeta) => void;
  /** 备注保存成功后触发，供父级重新拉取账号列表（卡片自身不持有列表状态）。 */
  onNoteSaved?: () => void;
  onCheckin?: (a: AccountMeta) => void;
  onRefresh?: (a: AccountMeta) => void;
  /** 领养 Buddy（仅领养，不派猫；与「一键旅行」的重叠部分单独暴露出来） */
  onAdopt?: (a: AccountMeta) => void;
  onSwitch?: (a: AccountMeta) => void;
  todayCheckedIn?: boolean;
  /** 今日旅行状态（undefined=查询中/未知，不渲染标签） */
  travelStatus?: TravelStatus;
  credit?: CreditExpiry;
  creditLoading?: boolean;
  /** 该账号积分最近一次查询完成时间（时间戳） */
  creditUpdatedAt?: number;
  creditPriority?: boolean;
  workbuddyActive?: boolean;
  codebuddyCliConfigured?: boolean;
  codebuddyCliActive?: boolean;
  /** 任一 CodeBuddy CLI 账号切换正在进行，用于阻止并发切换。 */
  codebuddyCliBusy?: boolean;
  onSwitchCodebuddyCli?: (a: AccountMeta) => void;
  /** 当前卡片是否为正在切换的目标账号。 */
  codebuddyCliLoading?: boolean;
  /** CodeBuddy CN IDE 是否已安装（可切换）。 */
  codebuddyCnIdeAvailable?: boolean;
  codebuddyCnIdeActive?: boolean;
  codebuddyCnIdeBusy?: boolean;
  codebuddyCnIdeLoading?: boolean;
  onSwitchCodebuddyCnIde?: (a: AccountMeta) => void;
  featuresDisabled?: boolean;
  /** 紧凑模式：头部缩成一条、按钮图标化、无 footer */
  compact?: boolean;
}

function ProductCurrentState({ product, compact = false }: { product: "workbuddy" | "codebuddy" | "codebuddy-cn"; compact?: boolean }) {
  const title =
    product === "workbuddy"
      ? "WorkBuddy 当前账号"
      : product === "codebuddy-cn"
        ? "CodeBuddy IDE 当前账号"
        : "CodeBuddy CLI 当前账号";
  return (
    <span
      role="status"
      aria-label={title}
      title={title}
      className={cn(
        "inline-flex items-center gap-2 rounded-full border border-primary/25 bg-primary/10 px-2.5 text-primary shadow-[inset_0_1px_0_rgba(255,255,255,.8)]",
        compact ? "h-7 text-xs" : "h-9",
      )}
    >
      {product === "workbuddy" ? (
        <WorkBuddyMark size={compact ? 18 : 22} />
      ) : product === "codebuddy-cn" ? (
        <CodeBuddyCnIdeMark size={compact ? 18 : 22} />
      ) : (
        <CodeBuddyMark size={compact ? 18 : 22} />
      )}
      <Check className={compact ? "size-3.5" : "size-4"} strokeWidth={2.25} />
    </span>
  );
}

export function AccountCard({ account, onDelete, onNoteSaved, onCheckin, onRefresh, onAdopt, onSwitch, todayCheckedIn, travelStatus, credit, creditLoading, creditUpdatedAt, creditPriority, workbuddyActive, codebuddyCliConfigured, codebuddyCliActive, codebuddyCliBusy, onSwitchCodebuddyCli, codebuddyCliLoading, codebuddyCnIdeAvailable, codebuddyCnIdeActive, codebuddyCnIdeBusy, codebuddyCnIdeLoading, onSwitchCodebuddyCnIde, featuresDisabled = true, compact = false }: Props) {
  const [resourcesOpen, setResourcesOpen] = useState(false);
  /** 备注编辑弹窗；`noteDraft` 是受控输入（打开时用当前备注初始化）。 */
  const [noteOpen, setNoteOpen] = useState(false);
  const [noteDraft, setNoteDraft] = useState("");
  const [noteSaving, setNoteSaving] = useState(false);
  /** 账号详情弹窗：展示本地记录里能看出「这是谁的号」的全部字段。 */
  const [detailOpen, setDetailOpen] = useState(false);
  const name = account.nickname || account.uid || "未命名账号";
  const expired = typeof account.expiresAt === "number" && account.expiresAt < Date.now();
  const avatarClass = avatarTone(name);
  const resources = creditResources(credit);
  const visibleResources = resources.slice(0, 2);
  const expiringAmount = credit?.ok ? credit.expiringSoonRemaining ?? 0 : 0;
  /** 弹窗内展示还有剩余的资源包（已用完的隐藏），按到期时间升序 */
  const allResources = (credit?.resources ?? [])
    .filter((resource) => resource.remaining > 0)
    .map((resource, index) => ({ resource, index }))
    .sort((left, right) => {
      const leftExpiry = left.resource.expireAt ?? Number.POSITIVE_INFINITY;
      const rightExpiry = right.resource.expireAt ?? Number.POSITIVE_INFINITY;
      return leftExpiry === rightExpiry ? left.index - right.index : leftExpiry - rightExpiry;
    })
    .map(({ resource }) => resource);

  const activeProductCount = [workbuddyActive, codebuddyCliActive, codebuddyCnIdeActive].filter(Boolean).length;

  /** 保存备注（空串 = 清除），成功后关闭弹窗并让父级刷新列表。 */
  async function submitNote() {
    setNoteSaving(true);
    try {
      await api.setAccountNote(account.id, noteDraft.trim());
      toast.success(noteDraft.trim() ? "备注已保存" : "备注已清除");
      setNoteOpen(false);
      onNoteSaved?.();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setNoteSaving(false);
    }
  }

  const statusChips = (
    <>
      {/* 备注放在最前面：它是用户自己起的标签，正是用来「一眼认出这是谁的号」的，
          排在区域/签到等自动状态之前才符合使用意图。 */}
      {account.note ? (
        <Badge variant="outline" className={cn(chipClass, "max-w-[12rem] gap-1")} title={`备注：${String(account.note)}`}>
          <PencilLine className="size-3 shrink-0" />
          <span className="truncate">{String(account.note)}</span>
        </Badge>
      ) : null}
      {regionChip(account)}
      {todayCheckedIn !== undefined && (
        <Badge variant={todayCheckedIn ? "success" : "secondary"} className={cn(chipClass, !todayCheckedIn && "text-muted-foreground")}><CircleCheck /> {todayCheckedIn ? "已签到" : "未签到"}</Badge>
      )}
      {travelChip(travelStatus)}
      {(account.needsRelogin || expired) && <Badge variant="warning" className={chipClass}>{account.needsRelogin ? "需重新登录" : "Token 已过期"}</Badge>}
      {creditPriority && (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="warning" className={cn(chipClass, "px-1")} aria-label="建议优先">
              <Star className="size-3.5" />
            </Badge>
          </TooltipTrigger>
          <TooltipContent side="top">建议优先使用</TooltipContent>
        </Tooltip>
      )}
      {!compact && activeProductCount >= 2 && <Badge variant="secondary" className={cn(chipClass, "text-muted-foreground")}>{activeProductCount} 个工具正在使用</Badge>}
    </>
  );

  return (
    <TooltipProvider>
      <article className="flex min-w-0 flex-col overflow-hidden rounded-2xl border border-border bg-card shadow-[0_1px_2px_rgba(15,23,42,.025),0_10px_28px_rgba(15,23,42,.035)] transition-shadow hover:shadow-[0_2px_4px_rgba(15,23,42,.04),0_14px_34px_rgba(15,23,42,.055)]">
      <header
        className={cn(
          "relative flex items-center border-b border-border",
          compact ? "min-h-[52px] px-3.5 py-1.5" : "min-h-[104px] px-5 py-3",
          workbuddyActive ? "bg-primary/5" : codebuddyCliActive ? "bg-muted/60" : "bg-muted/30",
        )}
      >
        <div className="pointer-events-none absolute inset-0 overflow-hidden">
          <div
            className={cn(
              "absolute -right-10 -top-16 rounded-full blur-2xl",
              compact ? "size-20" : "size-24",
              workbuddyActive ? "bg-primary/15" : codebuddyCliActive ? "bg-muted/50" : "bg-muted/30",
            )}
          />
          {workbuddyActive && (
            <div className={cn("absolute top-[64%] -translate-y-1/2 opacity-[0.075] saturate-50 grayscale-[10%]", codebuddyCliActive ? "right-[68px] rotate-[8deg]" : "right-5 rotate-[7deg]")}>
              <WorkBuddyMark size={compact ? 40 : 56} />
            </div>
          )}
          {codebuddyCliActive && (
            <div className={cn("absolute top-[63%] -translate-y-1/2 opacity-[0.065] saturate-50 grayscale-[18%]", workbuddyActive ? "right-1 -rotate-[8deg]" : "right-5 -rotate-[7deg]")}>
              <CodeBuddyMark size={compact ? 38 : 54} />
            </div>
          )}
        </div>

        <div className={cn("absolute z-20", compact ? "right-2.5 top-1/2 -translate-y-1/2" : "right-3.5 top-3.5")}>
          {demoModeEnabled ? (
            <DemoAction>
              <Button variant="ghost" size="icon" className={cn("rounded-lg text-muted-foreground hover:text-foreground", compact ? "size-7" : "size-8")} aria-label={`管理账号 ${name}`} title="更多账号操作">
                <Ellipsis />
              </Button>
            </DemoAction>
          ) : (
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="ghost" size="icon" className={cn("rounded-lg text-muted-foreground hover:text-foreground", compact ? "size-7" : "size-8")} aria-label={`管理账号 ${name}`} title="更多账号操作">
                  <Ellipsis />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-40">
                <DropdownMenuItem disabled={featuresDisabled || !onRefresh} onSelect={() => onRefresh?.(account)}>
                  <RefreshCw />刷新 Token
                </DropdownMenuItem>
                {todayCheckedIn === false && (
                  <DropdownMenuItem disabled={featuresDisabled || !onCheckin} onSelect={() => onCheckin?.(account)}>
                    <CircleCheck />手动签到
                  </DropdownMenuItem>
                )}
                {/* 领养：措辞随已知状态变化，避免用户点了才发现"已经有猫"或"还不够轮次"。
                    「旅行巡检也会顺带领养」这点保留在菜单里说清，因为一键旅行确实覆盖它。 */}
                <DropdownMenuItem disabled={featuresDisabled || !onAdopt} onSelect={() => onAdopt?.(account)}>
                  <Cat />
                  {travelStatus?.label === "adopted"
                    ? "重新检查 Buddy"
                    : travelStatus?.label === "adopt-threshold"
                      ? "领养 Buddy（需先攒对话）"
                      : "领养 Buddy"}
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                {/* 备注：授权进来的账号常只带邮箱/手机号/随机 uid，看不出「这是谁的号」，
                    因此给一个自定义标签。文案随是否已有备注变化，避免用户以为要重填。 */}
                <DropdownMenuItem onSelect={() => setNoteOpen(true)}>
                  <PencilLine />
                  {account.note ? "修改备注" : "添加备注"}
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={() => setDetailOpen(true)}>
                  <Info />
                  查看账号详情
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem className="text-destructive focus:bg-destructive/5 focus:text-destructive" onSelect={() => onDelete(account)}>
                  <Trash2 />删除账号
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          )}
        </div>

        {compact ? (
          <div className="relative z-10 flex w-full min-w-0 items-center gap-2 pr-10">
            <h3 className="min-w-0 flex-1 truncate text-[13px] font-semibold leading-5" title={name}>{name}</h3>
            <div className="hidden shrink-0 items-center gap-1 min-[420px]:flex">{statusChips}</div>
            <div className="ml-auto flex shrink-0 items-center gap-1">
              {workbuddyActive ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="relative inline-flex size-7 items-center justify-center rounded-lg border border-primary/25 bg-primary/10 text-primary">
                      <WorkBuddyMark size={15} />
                      <span className="absolute -right-1 -top-1 flex size-3.5 items-center justify-center rounded-full bg-primary text-primary-foreground">
                        <Check className="size-2.5" strokeWidth={3} />
                      </span>
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="top">WorkBuddy 当前账号</TooltipContent>
                </Tooltip>
              ) : demoModeEnabled ? (
                <DemoAction>
                  <Button variant="outline" size="icon" className="size-7 rounded-lg" aria-label="设为 WorkBuddy 当前账号">
                    <WorkBuddyMark size={15} />
                  </Button>
                </DemoAction>
              ) : (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button variant="outline" size="icon" className="size-7 rounded-lg" disabled={featuresDisabled || !onSwitch} onClick={() => onSwitch?.(account)} aria-label="设为 WorkBuddy 当前账号">
                      <WorkBuddyMark size={15} />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">设为 WorkBuddy 当前账号（会重启 WorkBuddy）</TooltipContent>
                </Tooltip>
              )}
              {codebuddyCnIdeActive ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="relative inline-flex size-7 items-center justify-center rounded-lg border border-primary/25 bg-primary/10 text-primary">
                      <CodeBuddyCnIdeMark size={15} />
                      <span className="absolute -right-1 -top-1 flex size-3.5 items-center justify-center rounded-full bg-primary text-primary-foreground">
                        <Check className="size-2.5" strokeWidth={3} />
                      </span>
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="top">CodeBuddy IDE 当前账号</TooltipContent>
                </Tooltip>
              ) : (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button variant="outline" size="icon" className="relative size-7 rounded-lg" disabled={featuresDisabled || !codebuddyCnIdeAvailable || !onSwitchCodebuddyCnIde || codebuddyCnIdeBusy} onClick={() => onSwitchCodebuddyCnIde?.(account)} aria-label={codebuddyCnIdeLoading ? "正在切换 CodeBuddy IDE" : "切换到 CodeBuddy IDE"} aria-busy={codebuddyCnIdeLoading}>
                      {codebuddyCnIdeLoading ? <Loader2 className="size-3.5 animate-spin" /> : <CodeBuddyCnIdeMark size={15} />}
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">{codebuddyCnIdeAvailable ? "切换到 CodeBuddy IDE（会重启 IDE）" : "未检测到 CodeBuddy IDE"}</TooltipContent>
                </Tooltip>
              )}
              {codebuddyCliActive ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="relative inline-flex size-7 items-center justify-center rounded-lg border border-primary/25 bg-primary/10 text-primary">
                      <CodeBuddyMark size={15} />
                      <span className="absolute -right-1 -top-1 flex size-3.5 items-center justify-center rounded-full bg-primary text-primary-foreground">
                        <Check className="size-2.5" strokeWidth={3} />
                      </span>
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="top">CodeBuddy CLI 当前账号</TooltipContent>
                </Tooltip>
              ) : (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button variant="outline" size="icon" className="size-7 rounded-lg" disabled={featuresDisabled || !codebuddyCliConfigured || !onSwitchCodebuddyCli || codebuddyCliBusy} onClick={() => onSwitchCodebuddyCli?.(account)} aria-label={codebuddyCliLoading ? "正在切换 CodeBuddy CLI 当前账号" : "设为 CodeBuddy CLI 当前账号"} aria-busy={codebuddyCliLoading}>
                      {codebuddyCliLoading ? <Loader2 className="size-3.5 animate-spin" /> : <CodeBuddyMark size={15} />}
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">{codebuddyCliConfigured ? "设为 CodeBuddy CLI 当前账号" : "请先接入 CodeBuddy CLI"}</TooltipContent>
                </Tooltip>
              )}
            </div>
          </div>
        ) : (
          <div className={cn("relative z-10 flex w-full min-w-0 items-center gap-3", workbuddyActive || codebuddyCliActive ? "pr-[112px]" : "pr-10")}>
            <div className={cn("flex size-12 shrink-0 items-center justify-center rounded-full text-base font-semibold ring-4 ring-white/65", avatarClass)}>{name.charAt(0).toUpperCase()}</div>
            <div className="min-w-0 flex-1">
              <h3 className="truncate text-sm font-semibold leading-5" title={name}>{name}</h3>
              <p className="mt-0.5 truncate text-xs leading-5 text-muted-foreground" title={account.email || account.uid || account.id}>{accountIdentity(account)}</p>
              <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-1.5">{statusChips}</div>
            </div>
          </div>
        )}
      </header>

      <section className={cn("flex min-w-0 flex-1 flex-col", compact ? "px-3.5 pb-3 pt-3" : "px-5 pb-4 pt-4")}>
        {creditLoading ? (
          <div className="flex items-center gap-2 py-3 text-sm text-muted-foreground"><Loader2 className="size-4 animate-spin" />积分查询中…</div>
        ) : !credit ? (
          <div className="py-3 text-sm text-muted-foreground">等待积分数据…</div>
        ) : !credit.ok ? (
          <div className="flex min-w-0 items-center gap-2 py-3 text-sm text-destructive" title={credit.error}>
            <Coins className="size-4 shrink-0" />
            <span className="min-w-0 truncate">{credit.error || "积分查询失败"}</span>
          </div>
        ) : (
          <>
            <div className="flex items-baseline gap-x-3 gap-y-1">
              <span className="flex items-center gap-1.5">
                <Sparkles className="size-4 shrink-0 stroke-[1.75] text-muted-foreground" aria-hidden="true" />
                <strong className={cn("font-semibold leading-none tabular-nums tracking-[-0.025em]", compact ? "text-[20px]" : "text-[22px]")} style={{ fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif' }}>{formatCredits(credit.totalRemaining ?? 0)}</strong>
              </span>
              <span className={cn("text-muted-foreground", compact ? "text-[11px]" : "text-xs")}>{resources.length} 个积分包</span>
              <div className={cn("ml-auto flex items-center gap-1.5 text-muted-foreground", compact ? "text-[11px]" : "text-xs")} title={expiringAmount > 0 ? `${formatCredits(expiringAmount)} 积分将在 7 天内到期` : resources[0]?.expireAt ? `最近到期 ${formatCreditExpiry(resources[0].expireAt).replace(" 到期", "")}` : "当前积分长期有效"}>
                <Clock3 className="size-3.5 shrink-0" />
                <span className="whitespace-nowrap tabular-nums">{creditUpdatedAt ? `${formatCreditUpdatedAt(creditUpdatedAt)} 更新` : "—"}</span>
              </div>
            </div>

            <div className={cn("text-[11px] font-medium text-muted-foreground", compact ? "mt-3" : "mt-4")}>近期到期</div>
            <div className={cn(compact ? "mt-1.5 space-y-2" : "mt-2 space-y-2.5")}>
              {visibleResources.length > 0 ? visibleResources.map((resource, index) => {
                const resourceName = resource.packageName || resource.packageCode || "积分包";
                const ratio = resource.total > 0 ? Math.min(100, Math.max(0, (resource.remaining / resource.total) * 100)) : 0;
                return (
                  <div key={`${resource.packageCode ?? "resource"}-${resource.expireAt ?? "none"}-${index}`} className="min-w-0" title={`${resourceName} · 剩余 ${formatCredits(resource.remaining)} / ${formatCredits(resource.total)} · ${formatCreditExpiry(resource.expireAt)}`}>
                    <div className={cn("grid min-w-0 grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-3", compact ? "text-[11px]" : "text-xs")}>
                      <span className={cn("rounded-lg bg-muted/80 font-medium tabular-nums text-foreground", compact ? "px-1.5 py-0.5" : "px-2 py-1")}>{formatCredits(resource.remaining)} 积分</span>
                      <span className="truncate text-muted-foreground">{resourceName}</span>
                      <span className={cn("whitespace-nowrap tabular-nums", expiryClass(resource.expired, resource.expiringSoon))}>{formatCreditExpiry(resource.expireAt)}</span>
                    </div>
                    <div className={cn("h-1 overflow-hidden rounded-full bg-muted", compact ? "mt-1" : "mt-1.5")} aria-hidden="true">
                      <div className={cn("h-full rounded-full", resource.expiringSoon || resource.expired ? "bg-orange-500" : "bg-primary")} style={{ width: `${ratio}%` }} />
                    </div>
                  </div>
                );
              }) : <div className="py-1 text-[11px] text-muted-foreground">暂无可用积分</div>}
            </div>

            {resources.length > 2 && (
              <button type="button" className={cn("inline-flex w-fit items-center gap-1.5 font-medium text-primary transition-colors hover:text-primary/80 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/30", compact ? "mt-2 text-[11px]" : "mt-3 text-xs")} onClick={() => setResourcesOpen(true)}>
                查看全部积分包
                <ArrowRight className="size-3.5" />
              </button>
            )}
          </>
        )}
      </section>

      {!compact && (
        <footer className="flex flex-wrap items-center gap-2.5 border-t px-5 py-2.5">
          {workbuddyActive ? <ProductCurrentState product="workbuddy" compact /> : demoModeEnabled ? (
            <DemoAction>
              <Button variant="outline" size="sm" className="h-7 rounded-full px-2.5 pr-3.5 text-xs" aria-label="设为 WorkBuddy 当前账号">
                <WorkBuddyMark size={18} /><span>设为当前</span>
              </Button>
            </DemoAction>
          ) : (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="sm" className="h-7 rounded-full px-2.5 pr-3.5 text-xs" disabled={featuresDisabled || !onSwitch} onClick={() => onSwitch?.(account)} aria-label="设为 WorkBuddy 当前账号">
                  <WorkBuddyMark size={18} /><span>设为当前</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">设为 WorkBuddy 当前账号（会重启 WorkBuddy）</TooltipContent>
            </Tooltip>
          )}
          {codebuddyCnIdeActive ? <ProductCurrentState product="codebuddy-cn" compact /> : (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="sm" className="h-7 rounded-full px-2.5 pr-3.5 text-xs" disabled={featuresDisabled || !codebuddyCnIdeAvailable || !onSwitchCodebuddyCnIde || codebuddyCnIdeBusy} onClick={() => onSwitchCodebuddyCnIde?.(account)} aria-label={codebuddyCnIdeLoading ? "正在切换 CodeBuddy IDE" : "切换到 CodeBuddy IDE"} aria-busy={codebuddyCnIdeLoading}>
                  {codebuddyCnIdeLoading ? <Loader2 className="size-4 animate-spin" /> : <CodeBuddyCnIdeMark size={18} />}<span>{codebuddyCnIdeLoading ? "切换中…" : "IDE"}</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">{codebuddyCnIdeAvailable ? "切换到 CodeBuddy IDE（会重启 IDE）" : "未检测到 CodeBuddy IDE"}</TooltipContent>
            </Tooltip>
          )}
          {codebuddyCliActive ? <ProductCurrentState product="codebuddy" compact /> : (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="outline" size="sm" className="h-7 rounded-full px-2.5 pr-3.5 text-xs" disabled={featuresDisabled || !codebuddyCliConfigured || !onSwitchCodebuddyCli || codebuddyCliBusy} onClick={() => onSwitchCodebuddyCli?.(account)} aria-label={codebuddyCliLoading ? "正在切换 CodeBuddy CLI 当前账号" : "设为 CodeBuddy CLI 当前账号"} aria-busy={codebuddyCliLoading}>
                  {codebuddyCliLoading ? <Loader2 className="size-4 animate-spin" /> : <CodeBuddyMark size={18} />}<span>{codebuddyCliLoading ? "切换中…" : "CLI 当前"}</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">{codebuddyCliConfigured ? "设为 CodeBuddy CLI 当前账号" : "请先接入 CodeBuddy CLI"}</TooltipContent>
            </Tooltip>
          )}
        </footer>
      )}
      </article>

      <Dialog open={resourcesOpen} onOpenChange={setResourcesOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>全部积分包</DialogTitle>
            <DialogDescription>{name} · 共 {allResources.length} 个积分包</DialogDescription>
          </DialogHeader>
          {allResources.length === 0 ? (
            <div className="px-1 py-6 text-center text-sm text-muted-foreground">当前没有可展示的资源包。</div>
          ) : (
            <div className="max-h-[60vh] min-w-0 overflow-y-auto divide-y divide-border/60">
              {allResources.map((resource, index) => {
                const ratio = resource.total > 0 ? Math.min(100, Math.max(0, (resource.remaining / resource.total) * 100)) : 0;
                return (
                  <div key={`${resource.packageCode || resource.packageName || "resource"}-${index}`} className="min-w-0 py-3 first:pt-0 last:pb-0">
                    <div className="flex min-w-0 items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium">{resource.packageName || resource.packageCode || "未命名资源包"}</div>
                        <div className="mt-1 text-[11px] text-muted-foreground">
                          {resource.expired ? "已到期" : resource.expiringSoon ? "7 天内到期" : resource.expireAt ? `到期 ${formatFullDate(resource.expireAt)}` : "长期有效"}
                        </div>
                      </div>
                      <div className="shrink-0 text-right text-xs">
                        <div className="font-medium">{formatCredits(resource.remaining)} / {formatCredits(resource.total)}</div>
                        <div className="mt-1 text-[11px] text-muted-foreground">已用 {formatCredits(resource.used)}</div>
                      </div>
                    </div>
                    <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted" aria-hidden="true">
                      <div className={cn("h-full rounded-full", resource.expired ? "bg-destructive/60" : resource.expiringSoon ? "bg-orange-500/80" : "bg-primary/75")} style={{ width: `${ratio}%` }} />
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </DialogContent>
      </Dialog>

      {/* 备注编辑：让用户给账号起个自己认得出的名字。
          空串 = 清除（后端会删掉该字段，而不是留一个空值）。 */}
      <Dialog
        open={noteOpen}
        onOpenChange={(open) => {
          if (open) setNoteDraft(account.note ? String(account.note) : "");
          setNoteOpen(open);
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>账号备注</DialogTitle>
            <DialogDescription>
              {name} · 备注只存在本机，用于区分「这是谁的号」；留空即清除。
            </DialogDescription>
          </DialogHeader>
          <Input
            value={noteDraft}
            onChange={(event) => setNoteDraft(event.target.value)}
            placeholder="例如：公司号 / 备用 / 张三"
            maxLength={40}
            spellCheck={false}
            autoComplete="off"
            onKeyDown={(event) => {
              // 回车即保存：备注是短文本，多一步点按钮没有意义。
              if (event.key === "Enter" && !noteSaving) {
                event.preventDefault();
                void submitNote();
              }
            }}
          />
          <DialogFooter>
            <Button variant="outline" onClick={() => setNoteOpen(false)} disabled={noteSaving}>
              取消
            </Button>
            <Button onClick={() => void submitNote()} disabled={noteSaving || noteDraft.trim() === (account.note ? String(account.note) : "")}>
              {noteSaving ? <Loader2 className="animate-spin" /> : <Save />}
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 账号详情：把本地记录里能回答「这是谁的号」的字段集中展示。
          此前卡片只显示昵称 + uid/邮箱，用户看不出授权的是哪个账号。 */}
      <Dialog open={detailOpen} onOpenChange={setDetailOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>账号详情</DialogTitle>
            <DialogDescription>{name}</DialogDescription>
          </DialogHeader>
          <div className="min-w-0 divide-y divide-border/60">
            {accountDetailRows(account).map(([label, value, hint]) => (
              <div key={label} className="flex min-w-0 items-start justify-between gap-4 py-2.5 first:pt-0 last:pb-0">
                <div className="shrink-0 text-xs text-muted-foreground" title={hint}>
                  {label}
                </div>
                <div className="min-w-0 flex-1 text-right">
                  {value ? (
                    <span className="break-all font-mono text-xs">{value}</span>
                  ) : (
                    <span className="text-xs text-muted-foreground/60">—</span>
                  )}
                </div>
              </div>
            ))}
          </div>
          <DialogFooter>
            {/* 复制 UID：排查问题时常要把它贴给别人，比手抄可靠。 */}
            <Button
              variant="outline"
              onClick={() => {
                void navigator.clipboard.writeText(account.uid || account.id);
                toast.success("已复制账号标识");
              }}
            >
              <Copy />
              复制 UID
            </Button>
            <Button onClick={() => setDetailOpen(false)}>关闭</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </TooltipProvider>
  );
}
