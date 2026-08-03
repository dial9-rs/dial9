// Trace-wide catalogue of custom-event fields that can feed dynamic charts.
// Built lazily and cached per loaded trace so the UI can browse every source
// without first finding and selecting an individual event marker.

import type { CustomTraceEvent } from "../../lib/trace/index.js";
import type { FieldChartKind } from "../../types/state.js";
import {
  fieldChartKindFromAnnotation,
  isChartableNumericValue,
  isFieldChartNameSupported,
} from "./field-chart-model.js";

export interface FieldChartSource {
  eventName: string;
  fieldName: string;
  /** Recognized, unambiguous schema annotation; null means ask the user. */
  kind: FieldChartKind | null;
}

export interface FieldChartSourceGroup {
  eventName: string;
  fields: readonly FieldChartSource[];
}

export interface FieldChartCatalog {
  /** Annotation-backed sources, shown first. */
  annotated: readonly FieldChartSourceGroup[];
  /** Other observed numeric sources, hidden behind a disclosure by default. */
  other: readonly FieldChartSourceGroup[];
}

interface Candidate {
  fieldName: string;
  numericSeen: boolean;
  kinds: Set<FieldChartKind>;
}

function compareName(a: string, b: string): number {
  return a.localeCompare(b);
}

/**
 * Inventory unique event/field sources in one pass over the custom-event
 * stream. An annotation is useful even when every optional value in this
 * particular trace is absent. Conflicting recognized annotations are not
 * guessed: a source with numeric observations falls back to the unannotated
 * group, while an empty conflicting source is omitted.
 */
export function buildFieldChartCatalog(
  events: readonly CustomTraceEvent[],
): FieldChartCatalog {
  const byEvent = new Map<string, Map<string, Candidate>>();

  for (const event of events) {
    if (!isFieldChartNameSupported(event.name)) continue;
    const eventFields = event.fields ?? {};
    const fieldNames = Object.keys(eventFields);
    for (const annotatedField of Object.keys(event.fieldKinds ?? {})) {
      if (!Object.hasOwn(eventFields, annotatedField)) {
        fieldNames.push(annotatedField);
      }
    }
    for (const fieldName of fieldNames) {
      if (!isFieldChartNameSupported(fieldName)) continue;
      const kind = fieldChartKindFromAnnotation(event.fieldKinds?.[fieldName]);
      const numeric = isChartableNumericValue(eventFields[fieldName] ?? null);
      if (kind === null && !numeric) continue;

      let candidatesByField = byEvent.get(event.name);
      if (candidatesByField === undefined) {
        candidatesByField = new Map();
        byEvent.set(event.name, candidatesByField);
      }
      let candidate = candidatesByField.get(fieldName);
      if (candidate === undefined) {
        candidate = { fieldName, numericSeen: false, kinds: new Set() };
        candidatesByField.set(fieldName, candidate);
      }
      candidate.numericSeen ||= numeric;
      if (kind !== null) candidate.kinds.add(kind);
    }
  }

  const annotated: FieldChartSourceGroup[] = [];
  const other: FieldChartSourceGroup[] = [];
  for (const eventName of [...byEvent.keys()].sort(compareName)) {
    const candidates = [...byEvent.get(eventName)!.values()].sort((a, b) =>
      compareName(a.fieldName, b.fieldName),
    );
    const annotatedFields: FieldChartSource[] = [];
    const otherFields: FieldChartSource[] = [];
    for (const candidate of candidates) {
      const kind =
        candidate.kinds.size === 1 ? [...candidate.kinds][0]! : null;
      const source = { eventName, fieldName: candidate.fieldName, kind };
      if (kind !== null) annotatedFields.push(source);
      else if (candidate.numericSeen) otherFields.push(source);
    }
    if (annotatedFields.length > 0) {
      annotated.push({ eventName, fields: annotatedFields });
    }
    if (otherFields.length > 0) other.push({ eventName, fields: otherFields });
  }

  return { annotated, other };
}

/** Case-insensitive event/field filter for the global source picker. */
export function filterFieldChartCatalog(
  catalog: FieldChartCatalog,
  query: string,
): FieldChartCatalog {
  const needle = query.trim().toLowerCase();
  if (needle === "") return catalog;

  const filterGroups = (
    groups: readonly FieldChartSourceGroup[],
  ): FieldChartSourceGroup[] =>
    groups.flatMap((group) => {
      if (group.eventName.toLowerCase().includes(needle)) return [group];
      const fields = group.fields.filter((field) =>
        field.fieldName.toLowerCase().includes(needle),
      );
      return fields.length > 0 ? [{ eventName: group.eventName, fields }] : [];
    });

  return {
    annotated: filterGroups(catalog.annotated),
    other: filterGroups(catalog.other),
  };
}

export function fieldChartCatalogSize(catalog: FieldChartCatalog): number {
  const count = (groups: readonly FieldChartSourceGroup[]): number =>
    groups.reduce((sum, group) => sum + group.fields.length, 0);
  return count(catalog.annotated) + count(catalog.other);
}
