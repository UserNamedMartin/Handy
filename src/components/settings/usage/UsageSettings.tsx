import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type UsageBucket, type UsageSummary } from "@/bindings";
import { SettingsGroup } from "../../ui/SettingsGroup";

/** Day windows offered by the activity chart. */
const RANGES = [7, 30, 90] as const;
type Range = (typeof RANGES)[number];

const formatDuration = (seconds: number): string => {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = seconds / 60;
  if (minutes < 60) return `${minutes.toFixed(minutes < 10 ? 1 : 0)}m`;
  const hours = Math.floor(minutes / 60);
  const rest = Math.round(minutes % 60);
  return rest === 0 ? `${hours}h` : `${hours}h ${rest}m`;
};

/**
 * Display name for a model id.
 *
 * Catalog ids can be a whole HF path
 * (`handy-computer/whisper-large-v3-gguf/whisper-large-v3-Q5_K_M.gguf`), which
 * left unchecked eats the row and squeezes the numbers out of the table. The
 * last path segment is the part that identifies the model; the full id stays in
 * the cell's tooltip.
 */
const modelLabel = (id: string): string => id.split("/").pop() || id;

/** Sub-cent spend still deserves a number rather than a bare "$0.00". */
const formatCost = (usd: number): string =>
  usd === 0 ? "$0" : usd < 0.01 ? "<$0.01" : `$${usd.toFixed(2)}`;

const Stat: React.FC<{ label: string; value: string; hint?: string }> = ({
  label,
  value,
  hint,
}) => (
  <div className="flex-1 min-w-[7.5rem] rounded-lg bg-mid-gray/10 px-3 py-2.5">
    <div className="text-xs text-text/50">{label}</div>
    <div className="text-lg font-medium text-text tabular-nums">{value}</div>
    {hint && <div className="text-[0.7rem] text-text/40">{hint}</div>}
  </div>
);

/**
 * Fill in the days the database has no row for.
 *
 * A day with no dictations is genuinely zero, but SQL `GROUP BY` simply omits
 * it — plotting the rows as-is would silently compress the time axis and make a
 * quiet week look busy.
 */
const fillDays = (buckets: UsageBucket[], days: number): UsageBucket[] => {
  const byPeriod = new Map(buckets.map((b) => [b.period, b]));
  const out: UsageBucket[] = [];
  const today = new Date();
  for (let i = days - 1; i >= 0; i--) {
    const day = new Date(today);
    day.setDate(today.getDate() - i);
    // Local calendar date, matching SQLite's 'localtime' modifier.
    const period = `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, "0")}-${String(day.getDate()).padStart(2, "0")}`;
    out.push(
      byPeriod.get(period) ?? {
        period,
        dictations: 0,
        seconds: 0,
        cost_usd: 0,
        measured: 0,
      },
    );
  }
  return out;
};

const DailyChart: React.FC<{ buckets: UsageBucket[] }> = ({ buckets }) => {
  const { t } = useTranslation();
  const [hover, setHover] = useState<number | null>(null);
  const peak = Math.max(...buckets.map((b) => b.seconds), 1);
  const active = hover !== null ? buckets[hover] : null;

  return (
    <div>
      <div className="flex items-end gap-[2px] h-28">
        {buckets.map((bucket, index) => {
          const height = (bucket.seconds / peak) * 100;
          return (
            <div
              key={bucket.period}
              className="flex-1 h-full flex items-end min-w-[2px]"
              onMouseEnter={() => setHover(index)}
              onMouseLeave={() => setHover(null)}
              title={`${bucket.period} · ${formatDuration(bucket.seconds)} · ${bucket.dictations}`}
            >
              <div
                className={`w-full rounded-sm transition-colors ${
                  hover === index ? "bg-logo-primary" : "bg-logo-primary/40"
                }`}
                // Keep an empty day visible as a hairline so the axis reads as
                // continuous rather than gappy.
                style={{ height: `${Math.max(height, bucket.seconds > 0 ? 2 : 1)}%` }}
              />
            </div>
          );
        })}
      </div>
      <div className="mt-1.5 flex justify-between text-[0.7rem] text-text/40 tabular-nums">
        <span>{buckets[0]?.period}</span>
        <span className="text-text/60">
          {active
            ? `${active.period} · ${formatDuration(active.seconds)} · ${active.dictations} ${t("settings.usage.dictations")}`
            : ""}
        </span>
        <span>{buckets[buckets.length - 1]?.period}</span>
      </div>
    </div>
  );
};

export const UsageSettings: React.FC = () => {
  const { t } = useTranslation();
  const [range, setRange] = useState<Range>(30);
  const [daily, setDaily] = useState<UsageBucket[]>([]);
  const [monthly, setMonthly] = useState<UsageBucket[]>([]);
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const [d, m, s] = await Promise.all([
        commands.getUsageDaily(90),
        commands.getUsageMonthly(12),
        commands.getUsageSummary(),
      ]);
      if (cancelled) return;
      if (d.status === "ok") setDaily(d.data);
      if (m.status === "ok") setMonthly(m.data);
      if (s.status === "ok") setSummary(s.data);
      else setError(s.error);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const windowed = useMemo(() => fillDays(daily, range), [daily, range]);

  const windowTotals = useMemo(
    () =>
      windowed.reduce(
        (acc, b) => ({
          dictations: acc.dictations + b.dictations,
          seconds: acc.seconds + b.seconds,
          cost: acc.cost + b.cost_usd,
        }),
        { dictations: 0, seconds: 0, cost: 0 },
      ),
    [windowed],
  );

  const thisMonth = monthly[monthly.length - 1];
  const paid = summary?.per_model.filter((m) => m.engine === "cloud") ?? [];
  // Older entries predate usage recording; averaging over all of them would
  // understate the true average length.
  const avgSeconds =
    summary && summary.measured > 0 ? summary.seconds / summary.measured : 0;
  const unmeasured = summary ? summary.dictations - summary.measured : 0;

  if (error) {
    return (
      <SettingsGroup title={t("settings.usage.title")}>
        <div className="px-3 py-2 text-sm text-text/60">{error}</div>
      </SettingsGroup>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <SettingsGroup title={t("settings.usage.overview")}>
        <div className="flex flex-wrap gap-2 px-3 py-3">
          <Stat
            label={t("settings.usage.totalDictations")}
            value={summary ? summary.dictations.toLocaleString() : "—"}
          />
          <Stat
            label={t("settings.usage.totalTime")}
            value={summary ? formatDuration(summary.seconds) : "—"}
          />
          <Stat
            label={t("settings.usage.averageLength")}
            value={avgSeconds ? formatDuration(avgSeconds) : "—"}
          />
          <Stat
            label={t("settings.usage.thisMonth")}
            value={thisMonth ? formatCost(thisMonth.cost_usd) : "$0"}
            hint={thisMonth ? formatDuration(thisMonth.seconds) : undefined}
          />
        </div>
        {unmeasured > 0 && (
          <div className="px-3 pb-3 text-[0.7rem] text-text/40">
            {t("settings.usage.unmeasured", { count: unmeasured })}
          </div>
        )}
      </SettingsGroup>

      <SettingsGroup title={t("settings.usage.activity")}>
        <div className="px-3 py-3">
          <div className="mb-3 flex items-center justify-between">
            <div className="text-xs text-text/50">
              {formatDuration(windowTotals.seconds)} ·{" "}
              {windowTotals.dictations} {t("settings.usage.dictations")}
              {windowTotals.cost > 0 && ` · ${formatCost(windowTotals.cost)}`}
            </div>
            <div className="flex gap-1">
              {RANGES.map((option) => (
                <button
                  key={option}
                  onClick={() => setRange(option)}
                  className={`px-2 py-0.5 rounded text-xs transition-colors cursor-pointer ${
                    range === option
                      ? "bg-logo-primary/20 text-logo-primary"
                      : "text-text/50 hover:text-text"
                  }`}
                >
                  {t("settings.usage.days", { count: option })}
                </button>
              ))}
            </div>
          </div>
          <DailyChart buckets={windowed} />
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.usage.byModel")}>
        <div className="px-3 py-2">
          {/* Fixed layout: the model name must yield to the numbers, not the
              other way round — auto layout let one long id collapse them. */}
          {summary && summary.per_model.length > 0 ? (
            <table className="w-full text-sm table-fixed">
              <thead>
                <tr className="text-xs text-text/40 text-left">
                  <th className="font-normal py-1">{t("settings.usage.model")}</th>
                  <th className="font-normal py-1 text-right w-[4.5rem]">
                    {t("settings.usage.dictations")}
                  </th>
                  <th className="font-normal py-1 text-right w-[4rem]">
                    {t("settings.usage.time")}
                  </th>
                  <th className="font-normal py-1 text-right w-[4rem]">
                    {t("settings.usage.cost")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {summary.per_model.map((model) => (
                  <tr key={`${model.model_id}:${model.engine}`} className="border-t border-mid-gray/20">
                    <td className="py-1.5 pr-3 min-w-0">
                      <div className="flex items-center gap-1.5 min-w-0">
                        <span
                          className="text-text truncate"
                          title={model.model_id}
                        >
                          {model.model_id === "unknown"
                            ? t("settings.usage.beforeTracking")
                            : modelLabel(model.model_id)}
                        </span>
                        {model.engine === "cloud" && (
                          <span className="shrink-0 text-[0.65rem] px-1 py-px rounded bg-logo-primary/10 text-logo-primary">
                            {t("settings.usage.cloud")}
                          </span>
                        )}
                      </div>
                    </td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {model.dictations}
                    </td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {formatDuration(model.seconds)}
                    </td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {model.engine === "cloud" ? formatCost(model.cost_usd) : "—"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <div className="py-2 text-sm text-text/50">{t("settings.usage.empty")}</div>
          )}
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.usage.spend")}>
        <div className="px-3 py-2">
          {paid.length === 0 ? (
            <div className="py-2 text-sm text-text/50">{t("settings.usage.noSpend")}</div>
          ) : (
            <table className="w-full text-sm table-fixed">
              <thead>
                <tr className="text-xs text-text/40 text-left">
                  <th className="font-normal py-1">{t("settings.usage.month")}</th>
                  <th className="font-normal py-1 text-right w-[4.5rem]">
                    {t("settings.usage.dictations")}
                  </th>
                  <th className="font-normal py-1 text-right w-[4rem]">
                    {t("settings.usage.time")}
                  </th>
                  <th className="font-normal py-1 text-right w-[4rem]">
                    {t("settings.usage.cost")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {[...monthly].reverse().map((month) => (
                  <tr key={month.period} className="border-t border-mid-gray/20">
                    <td className="py-1.5 tabular-nums text-text">{month.period}</td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {month.dictations}
                    </td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {formatDuration(month.seconds)}
                    </td>
                    <td className="py-1.5 text-right tabular-nums text-text/60">
                      {formatCost(month.cost_usd)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <div className="mt-2 pt-2 border-t border-mid-gray/20 text-[0.7rem] text-text/40">
            {t("settings.usage.estimateNote")}
          </div>
        </div>
      </SettingsGroup>
    </div>
  );
};
