-- Memory v2 — schema v5 (per-procedure latency tracking)
--
-- Adds `last_latency_ms` to the procedures table. `record_use` writes the
-- latency measured by `ProceduralSkillTelemetrySink` into this column so
-- per-skill latency-regression detection sees real values instead of the
-- zeros it observed while the measured latency was underscore-ignored.
--
-- Existing rows default to 0 (no timed use recorded yet), matching the
-- `Procedure::last_latency_ms` default used at the construction sites that
-- have no latency to report.

ALTER TABLE procedures ADD COLUMN last_latency_ms INTEGER NOT NULL DEFAULT 0;
