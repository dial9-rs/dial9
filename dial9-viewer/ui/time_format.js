"use strict";

// Shared time presentation helpers for the viewer pages. Displayed dates use
// YYYY-MM-DD and displayed clocks always use a 24-hour representation. Native
// datetime-local inputs deliberately keep their required YYYY-MM-DDTHH:mm:ss
// value format; these helpers convert between that input value and an instant.
(function (exports) {
  function pad2(value) {
    return String(value).padStart(2, "0");
  }

  function parts(date, useLocalTz) {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return null;
    if (useLocalTz) {
      return {
        year: date.getFullYear(),
        month: date.getMonth() + 1,
        day: date.getDate(),
        hour: date.getHours(),
        minute: date.getMinutes(),
        second: date.getSeconds(),
      };
    }
    return {
      year: date.getUTCFullYear(),
      month: date.getUTCMonth() + 1,
      day: date.getUTCDate(),
      hour: date.getUTCHours(),
      minute: date.getUTCMinutes(),
      second: date.getUTCSeconds(),
    };
  }

  function formatDate(date, useLocalTz) {
    const value = parts(date, useLocalTz);
    if (!value) return "";
    return `${value.year}-${pad2(value.month)}-${pad2(value.day)}`;
  }

  function formatTime(date, useLocalTz) {
    const value = parts(date, useLocalTz);
    if (!value) return "";
    return `${pad2(value.hour)}:${pad2(value.minute)}:${pad2(value.second)}`;
  }

  function formatDateTime(date, useLocalTz) {
    const dateText = formatDate(date, useLocalTz);
    const timeText = formatTime(date, useLocalTz);
    if (!dateText || !timeText) return "";
    return `${dateText} ${timeText} ${useLocalTz ? "Local" : "UTC"}`;
  }

  // The landing page uses the native datetime-local picker. Seconds are
  // intentionally omitted because its S3 browse window is minute-granular.
  function dateToRangePicker(date, useLocalTz) {
    const value = parts(date, useLocalTz);
    if (!value) return "";
    return `${value.year}-${pad2(value.month)}-${pad2(value.day)}T${pad2(value.hour)}:${pad2(value.minute)}`;
  }

  function rangePickerToDate(value, useLocalTz) {
    if (!value) return null;
    const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(value);
    if (!match) return null;
    const [, year, month, day, hour, minute] = match;
    const iso = `${year}-${month}-${day}T${hour}:${minute}`;
    const date = new Date(useLocalTz ? iso : iso + "Z");
    const parsed = parts(date, useLocalTz);
    if (!parsed || parsed.year !== Number(year) || parsed.month !== Number(month) ||
        parsed.day !== Number(day) || parsed.hour !== Number(hour) ||
        parsed.minute !== Number(minute)) {
      return null;
    }
    return date;
  }

  // A local native range value also cannot identify which repeated DST-fold hour it
  // represents. Keep the backing instant if its formatted picker text has not
  // changed; parse normally after the user edits the text.
  function rangePickerToDatePreserving(value, currentDate, useLocalTz) {
    if (currentDate instanceof Date && !Number.isNaN(currentDate.getTime()) &&
        value === dateToRangePicker(currentDate, useLocalTz)) {
      return new Date(currentDate.getTime());
    }
    return rangePickerToDate(value, useLocalTz);
  }

  function nsToPicker(ns, useLocalTz) {
    if (ns == null || ns === "") return "";
    const value = parts(new Date(Number(ns) / 1e6), useLocalTz);
    if (!value) return "";
    return `${value.year}-${pad2(value.month)}-${pad2(value.day)}T${pad2(value.hour)}:${pad2(value.minute)}:${pad2(value.second)}`;
  }

  function pickerToNs(value, useLocalTz) {
    if (!value) return null;
    const ms = new Date(useLocalTz ? value : value + "Z").getTime();
    return Number.isNaN(ms) ? null : Math.floor(ms * 1e6).toString();
  }

  // A local datetime value cannot identify which occurrence of a repeated DST
  // hour it represents. Preserve the known instant while its rendered picker
  // value is unchanged; once the user edits it, parse their new wall-clock
  // value normally.
  function pickerToNsPreserving(value, currentNs, useLocalTz) {
    if (currentNs != null && value === nsToPicker(currentNs, useLocalTz)) {
      return String(currentNs);
    }
    return pickerToNs(value, useLocalTz);
  }

  function timeModeLabel(useRelative, useLocalTz) {
    if (useRelative) return "Time: Relative";
    return useLocalTz ? "Time: Local" : "Time: UTC";
  }

  exports.formatDate = formatDate;
  exports.formatTime = formatTime;
  exports.formatDateTime = formatDateTime;
  exports.dateToRangePicker = dateToRangePicker;
  exports.rangePickerToDate = rangePickerToDate;
  exports.rangePickerToDatePreserving = rangePickerToDatePreserving;
  exports.nsToPicker = nsToPicker;
  exports.pickerToNs = pickerToNs;
  exports.pickerToNsPreserving = pickerToNsPreserving;
  exports.timeModeLabel = timeModeLabel;

  if (typeof window !== "undefined") window.Dial9TimeFormat = exports;
})(typeof exports === "undefined" ? {} : exports);
