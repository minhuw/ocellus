"use client";

import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { DashboardEntry } from "../lib/dashboard-types";

type DashboardPickerProps = {
  dashboards: DashboardEntry[];
  version: string;
};

type ArchitectureAlias = {
  name: string;
  family: string;
  model: string;
  stepping?: string;
};

const architectureAliases: Record<string, ArchitectureAlias[]> = {
  "Sandy Bridge-EP / Ivy Bridge-EP": [
    {
      name: "Sandy Bridge-EP",
      family: "6",
      model: "0x2d",
    },
    {
      name: "Ivy Bridge-EP",
      family: "6",
      model: "0x3e",
    },
  ],
  "Haswell / Broadwell Xeon": [
    {
      name: "Haswell-EP",
      family: "6",
      model: "0x3f",
    },
    {
      name: "Broadwell-EP",
      family: "6",
      model: "0x4f",
    },
  ],
  "Skylake / Cascade Lake Xeon": [
    {
      name: "Skylake-SP",
      family: "6",
      model: "0x55",
    },
    {
      name: "Cascade Lake-SP",
      family: "6",
      model: "0x55",
    },
  ],
  "Ice Lake Xeon": [
    {
      name: "Ice Lake-SP",
      family: "6",
      model: "0x6a",
    },
  ],
  "Sapphire Rapids / Emerald Rapids Xeon": [
    {
      name: "Sapphire Rapids",
      family: "6",
      model: "0x8f",
    },
    {
      name: "Emerald Rapids",
      family: "6",
      model: "0xcf",
    },
  ],
};

type DashboardOption = {
  id: string;
  architecture: ArchitectureAlias;
  dashboard: DashboardEntry;
};

function dashboardOptions(dashboards: DashboardEntry[]): DashboardOption[] {
  return dashboards.flatMap((dashboard) => {
    const architectures = architectureAliases[dashboard.architecture] ?? [
      {
        name: dashboard.architecture,
        family: "varies",
        model: "varies",
      },
    ];
    return architectures.map((architecture) => ({
      id: `${dashboard.uid}:${architecture.name}`,
      architecture,
      dashboard,
    }));
  });
}

function optionText(option: DashboardOption): string {
  return `${option.architecture.name} family ${option.architecture.family} model ${option.architecture.model} ${option.architecture.stepping ?? ""} ${option.dashboard.title} ${option.dashboard.architecture} ${option.dashboard.file}`;
}

function dashboardUrl(dashboard: DashboardEntry, versionMode: string): string {
  if (versionMode === "latest") {
    return dashboard.url;
  }
  return dashboard.versionedUrl ?? dashboard.releaseUrl ?? dashboard.url;
}

function classicDashboardUrl(dashboard: DashboardEntry, versionMode: string): string {
  if (versionMode === "latest") {
    return dashboard.classicUrl;
  }
  return (
    dashboard.classicVersionedUrl ??
    dashboard.classicReleaseUrl ??
    dashboard.classicUrl
  );
}

function dashboardActionPath(dashboard: DashboardEntry, versionMode: string): string {
  if (versionMode === "latest") {
    return `/dashboard-assets/v2/${dashboard.file}`;
  }
  return dashboard.versionedUrl
    ? new URL(dashboard.versionedUrl).pathname
    : `/dashboard-assets/v2/${dashboard.file}`;
}

function classicDashboardActionPath(
  dashboard: DashboardEntry,
  versionMode: string,
): string {
  if (versionMode === "latest") {
    return `/dashboard-assets/classic/${dashboard.classicFile}`;
  }
  return dashboard.classicVersionedUrl
    ? new URL(dashboard.classicVersionedUrl).pathname
    : `/dashboard-assets/classic/${dashboard.classicFile}`;
}

export function DashboardPicker({ dashboards, version }: DashboardPickerProps) {
  const options = useMemo(() => dashboardOptions(dashboards), [dashboards]);
  const searchInputId = useId();
  const versionSelectId = useId();
  const listboxId = useId();
  const searchRef = useRef<HTMLInputElement>(null);
  const [isOpen, setIsOpen] = useState(false);
  const [copiedItem, setCopiedItem] = useState<string | null>(null);
  const [copyError, setCopyError] = useState<string | null>(null);
  const [versionMode, setVersionMode] = useState("latest");
  const [selectedOptionId, setSelectedOptionId] = useState(options[0]?.id ?? "");
  const [query, setQuery] = useState(options[0]?.architecture.name ?? "");

  const selectedOption =
    options.find((option) => option.id === selectedOptionId) ?? options[0];

  const filteredOptions = useMemo(() => {
    const normalizedQuery = query.trim().toLowerCase();
    if (!normalizedQuery) {
      return options;
    }
    return options.filter((option) =>
      optionText(option).toLowerCase().includes(normalizedQuery),
    );
  }, [options, query]);

  const selectedDashboard = selectedOption?.dashboard;
  const selectedDashboardUrl = selectedDashboard
    ? dashboardUrl(selectedDashboard, versionMode)
    : "";
  const selectedClassicDashboardUrl = selectedDashboard
    ? classicDashboardUrl(selectedDashboard, versionMode)
    : "";
  const selectedDashboardActionUrl = selectedDashboardUrl
    ? dashboardActionPath(selectedDashboard, versionMode)
    : "";
  const selectedClassicDashboardActionUrl = selectedClassicDashboardUrl
    ? classicDashboardActionPath(selectedDashboard, versionMode)
    : "";
  const selectedVersionLabel =
    versionMode === "latest" ? "Latest channel" : version;
  const canCopyClassicDashboard = versionMode === "latest";
  const hasPinnedVersion = dashboards.some((dashboard) => dashboard.versionedUrl);

  useEffect(() => {
    if (selectedOption && !isOpen) {
      setQuery(selectedOption.architecture.name);
    }
  }, [isOpen, selectedOption]);

  function selectOption(option: DashboardOption) {
    setSelectedOptionId(option.id);
    setQuery(option.architecture.name);
    setIsOpen(false);
    searchRef.current?.focus();
  }

  function toggleModelMenu() {
    if (isOpen) {
      setIsOpen(false);
      setQuery(selectedOption?.architecture.name ?? "");
      return;
    }
    setQuery("");
    setIsOpen(true);
    searchRef.current?.focus();
  }

  async function copyDashboardJson(url: string) {
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`failed to fetch dashboard JSON: ${response.status}`);
    }

    const contentType = response.headers.get("content-type") ?? "";
    const payload = await response.text();
    if (!contentType.includes("application/json") && !payload.trimStart().startsWith("{")) {
      throw new Error("dashboard endpoint returned non-JSON content");
    }

    await navigator.clipboard.writeText(payload);
    setCopiedItem(url);
    setCopyError(null);
    window.setTimeout(() => setCopiedItem(null), 1600);
  }

  function copyClassicDashboardJson(url: string) {
    void copyDashboardJson(url).catch((error) => {
      setCopyError((error as Error).message);
    });
  }

  return (
    <div className="dashboard-picker">
      <div className="dashboard-picker-controls">
        <div
          className="picker-field dashboard-combobox"
          onBlur={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget)) {
              setIsOpen(false);
            }
          }}
        >
          <label htmlFor={searchInputId}>Select CPU model</label>
          <div className="combobox-control">
            <input
              id={searchInputId}
              ref={searchRef}
              type="search"
              role="combobox"
              aria-haspopup="listbox"
              aria-expanded={isOpen}
              aria-controls={listboxId}
              aria-autocomplete="list"
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setIsOpen(true);
              }}
              onFocus={() => setIsOpen(true)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setIsOpen(false);
                  setQuery(selectedOption?.architecture.name ?? "");
                }
                if (event.key === "Enter" && filteredOptions[0]) {
                  event.preventDefault();
                  selectOption(filteredOptions[0]);
                }
              }}
              placeholder="Search by Xeon family, model, or dashboard"
            />
            <button
              type="button"
              className="combobox-toggle"
              aria-label={isOpen ? "Close CPU model list" : "Show CPU models"}
              aria-expanded={isOpen}
              aria-controls={listboxId}
              onMouseDown={(event) => event.preventDefault()}
              onClick={toggleModelMenu}
            >
              <span aria-hidden="true" />
            </button>
          </div>
          {isOpen ? (
            <div className="combobox-menu" id={listboxId} role="listbox">
              {filteredOptions.length > 0 ? (
                filteredOptions.map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    className="combobox-option"
                    aria-selected={option.id === selectedOption?.id}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => selectOption(option)}
                    role="option"
                  >
                    <span>{option.architecture.name}</span>
                    <small>
                      family {option.architecture.family} · model{" "}
                      {option.architecture.model}
                    </small>
                  </button>
                ))
              ) : (
                <p className="combobox-empty">No dashboards match that search.</p>
              )}
            </div>
          ) : null}
        </div>

        <label
          className="picker-field dashboard-version-field"
          htmlFor={versionSelectId}
        >
          <span>Version</span>
          <div className="select-control">
            <select
              id={versionSelectId}
              value={versionMode}
              onChange={(event) => setVersionMode(event.target.value)}
            >
              <option value="latest">Latest</option>
              <option value={version} disabled={!hasPinnedVersion}>
                {version}
                {hasPinnedVersion ? "" : " (local build)"}
              </option>
            </select>
            <span aria-hidden="true" />
          </div>
        </label>
      </div>

      {selectedDashboard ? (
        <article className="selected-dashboard">
          <div className="dashboard-card-header">
            <div>
              <h3>{selectedOption.architecture.name}</h3>
              <p>
                Family {selectedOption.architecture.family} · Model{" "}
                {selectedOption.architecture.model}
                {selectedOption.architecture.stepping
                  ? ` · Stepping ${selectedOption.architecture.stepping}`
                  : ""}
              </p>
            </div>
            <span>{selectedVersionLabel}</span>
          </div>

          <div className="dashboard-facts" aria-label="Dashboard details">
            <div>
              <span>Dashboard</span>
              <strong>{selectedDashboard.title}</strong>
            </div>
            <div>
              <span>Classic JSON</span>
              <strong>{Math.ceil(selectedDashboard.classicBytes / 1024)} KB</strong>
            </div>
            <div>
              <span>V2 Resource</span>
              <strong>{Math.ceil(selectedDashboard.bytes / 1024)} KB</strong>
            </div>
          </div>

          <div className="dashboard-downloads">
            <section className="dashboard-action-card primary-action">
              <div className="action-copy">
                <span>Recommended for the import dialog</span>
                <h4>Classic Import JSON</h4>
                <p>
                  Copy the dashboard body into Grafana&apos;s import text area,
                  or download it and upload the file.
                </p>
              </div>
              <div className="dashboard-actions">
                {canCopyClassicDashboard ? (
                  <button
                    type="button"
                    className="button primary"
                    onClick={() => copyClassicDashboardJson(selectedClassicDashboardActionUrl)}
                  >
                    {copiedItem === selectedClassicDashboardActionUrl
                      ? "Copied"
                      : "Copy JSON"}
                  </button>
                ) : null}
                <a
                  className={canCopyClassicDashboard ? "button secondary" : "button primary"}
                  href={selectedClassicDashboardActionUrl}
                  download={selectedDashboard.classicFile}
                >
                  Download
                </a>
              </div>
              {canCopyClassicDashboard ? null : (
                <p className="dashboard-note">
                  Pinned releases are downloaded from immutable release assets.
                  Download the JSON, then upload it in Grafana.
                </p>
              )}
              {copyError ? <p className="dashboard-error">{copyError}</p> : null}
            </section>

            <section className="dashboard-action-card">
              <div className="action-copy">
                <span>For Git Sync and file provisioning</span>
                <h4>Grafana V2 Resource JSON</h4>
                <p>
                  Keep this format for automated provisioning workflows and
                  version-controlled dashboard resources. Download the resource
                  file and place it in your provisioning path or repository.
                </p>
              </div>
              <div className="dashboard-actions">
                <a
                  className="button secondary"
                  href={selectedDashboardActionUrl}
                  download={selectedDashboard.file}
                >
                  Download
                </a>
              </div>
            </section>
          </div>
        </article>
      ) : (
        <p className="muted">No dashboards match that search.</p>
      )}
    </div>
  );
}
