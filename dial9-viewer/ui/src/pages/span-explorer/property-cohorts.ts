import { html, nothing } from "lit-html";
import type { TailReport } from "../../lib/trace/span-tail-analysis.js";
import { propertyValueColor } from "./categorical-histograms.js";

const number = (n: number) => n.toLocaleString();
const percent = (n: number) => `${(n * 100).toLocaleString(undefined, { maximumFractionDigits: 1 })}%`;
type Group = TailReport["groups"][number];

/**
 * Backend prevalence is an unrounded count/cohort-size ratio. Recover integer
 * counts for display, including recorded values whose count is zero.
 */
export function propertyCohortCounts(group: Group, cohort: "typical" | "slow", total: number) {
  if (group.values.some(value => !Number.isFinite(group[cohort][value]))) return null;
  const values = group.values.map(value => ({
    value,
    count: Math.round(group[cohort][value]! * total),
    share: group[cohort][value]!,
  }));
  const missing = Math.max(0, total - values.reduce((sum, value) => sum + value.count, 0));
  return { values, missing, missingShare: total > 0 ? missing / total : 0 };
}

/** Two comparable 100% bars for the property already displayed above them. */
export function renderPropertyCohorts(group: Group, report: TailReport, selected: boolean, valueOrder: readonly string[] = group.values) {
  const typical = propertyCohortCounts(group, "typical", report.typicalCount);
  const slow = propertyCohortCounts(group, "slow", report.slowCount);
  const rows = [
    { label: "Typical", total: report.typicalCount, data: typical },
    { label: selected ? "Selected" : "Slow", total: report.slowCount, data: slow },
  ];
  return html`<section class="tail-property-cohorts" aria-label="${group.key} proportions in typical and ${selected ? "selected" : "slow"} spans">
    <div class="tail-property-cohort-heading"><h4>Typical vs ${selected ? "selected" : "slow"} proportions</h4><span class="tail-note">${group.key}</span></div>
    ${rows.map(({ label, total, data }) => html`<div class="tail-property-cohort-row">
      <span class="tail-property-cohort-label">${label}<small>${number(total)} spans</small></span>
      ${total && data ? html`<div class="tail-property-cohort-bar" role="img" aria-label="${label}: ${data.values.map(v => `${v.value} ${number(v.count)} spans, ${percent(v.share)}`).join("; ")}; Missing ${number(data.missing)} spans">
        ${data.values.filter(v => v.count > 0).map(v => html`<span class="tail-property-cohort-segment"
          style="width:${v.share * 100}%;background:${propertyValueColor(valueOrder.indexOf(v.value))}"
          title="${v.value}: ${number(v.count)} of ${number(total)} spans (${percent(v.share)})">
          ${v.share >= .18 ? html`<b>${v.value}</b>` : nothing}${v.share >= .08 ? percent(v.share) : nothing}
        </span>`)}
        ${data.missing ? html`<span class="tail-property-cohort-segment missing" style="width:${data.missingShare * 100}%"
          title="Missing ${group.key}: ${number(data.missing)} of ${number(total)} spans (${percent(data.missingShare)})">
          ${data.missingShare >= .18 ? "Missing" : nothing}${data.missingShare >= .08 ? percent(data.missingShare) : nothing}
        </span>` : nothing}
      </div>` : html`<span class="tail-note">${total ? "Proportions unavailable" : "No matching spans"}</span>`}
    </div>`)}
    <div class="tail-property-cohort-legend">
      <span>Counts: typical / ${selected ? "selected" : "slow"}</span>
      ${group.values.map((value, i) => typical?.values[i]?.count === 0 && slow?.values[i]?.count === 0 ? nothing : html`<span><i style="background:${propertyValueColor(valueOrder.indexOf(value))}"></i><b>${value}</b>
        <span>${typical ? number(typical.values[i]!.count) : "unavailable"} / ${slow ? number(slow.values[i]!.count) : "unavailable"}</span></span>`)}
      ${typical?.missing === 0 && slow?.missing === 0 ? nothing : html`<span><i class="missing"></i><b>Missing</b><span>${typical ? number(typical.missing) : "unavailable"} / ${slow ? number(slow.missing) : "unavailable"}</span></span>`}
    </div>
  </section>`;
}
