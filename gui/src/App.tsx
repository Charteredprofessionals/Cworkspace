import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import {
  AlertCircle,
  ArrowDown,
  ArrowUpRight,
  BarChart3,
  Briefcase,
  CalendarClock,
  Check,
  ChevronDown,
  ChevronRight,
  CircleHelp,
  Clock3,
  FileDown,
  FileSpreadsheet,
  FileText,
  Folder,
  FolderPlus,
  History,
  KeyRound,
  LoaderCircle,
  PanelRightClose,
  Pencil,
  Play,
  Plus,
  Settings,
  ShieldCheck,
  Sparkles,
  StopCircle,
  Trash2,
  Wrench,
  X,
} from "lucide-react";
import DOMPurify from "dompurify";
import mammoth from "mammoth";
import ReactMarkdown from "react-markdown";
import * as XLSX from "xlsx";


import React from "react";

interface ErrorBoundaryState {
  hasError: boolean;
  error: Error | null;
}

class ErrorBoundary extends React.Component<
  { children: React.ReactNode; fallback?: React.ReactNode },
  ErrorBoundaryState
> {
  constructor(props: { children: React.ReactNode; fallback?: React.ReactNode }) {
    super(props);
    this.state = { hasError: false, error: null };
  }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, errorInfo: React.ErrorInfo) {
    console.error("ErrorBoundary caught:", error, errorInfo);
  }

  render() {
    if (this.state.hasError) {
      if (this.props.fallback) {
        return this.props.fallback;
      }
      return (
        <div className="error-boundary" style={{ padding: "20px", color: "#dc2626" }}>
          <h2>Something went wrong</h2>
          <pre style={{ whiteSpace: "pre-wrap", fontSize: "12px" }}>
            {this.state.error?.message}
            {this.state.error?.stack && "\n\n" + this.state.error.stack}
          </pre>
          <button onClick={() => this.setState({ hasError: false, error: null })}>
            Try again
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
type ModelRoute = {
  model: string;
  provider: string;
  available: boolean;
  detail: string;
};

type CoworkEvent = {
  event:
    | "AgentSpawned"
    | "ToolInvoked"
    | "ArtifactCreated"
    | "ApprovalRequired"
    | "TaskDelta"
    | "TaskCompleted";
  data: Record<string, unknown>;
};

type FeedEntry = {
  key: string;
  event: CoworkEvent["event"];
  data: Record<string, unknown>;
  createdAt: number;
};

type HistoryEntry = {
  taskId: string;
  goal: string;
  status: "running" | "completed" | "stopped" | "failed";
  updatedAt: number;
  model?: string;
  artifacts?: string[];
};

type Artifact = {
  path: string;
  mime_type?: string;
};

type ArtifactPayload = {
  name: string;
  mime_type: string;
  content_base64: string;
};

type PreviewState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; payload: ArtifactPayload; markdown?: string; docxHtml?: string; rows?: unknown[][]; columns?: number }
  | { status: "pdf"; payload: ArtifactPayload; url: string };

type TabStatus = "idle" | "running" | "completed" | "stopped" | "failed";

/// Interface Layout (Settings → General): the simplified Knowledge Work face
/// for research/writing/everyday tasks, or the full Software Engineering
/// interface for coding and agent management.
type InterfaceLayout = "knowledge" | "software";

type SessionTab = {
  id: string;
  title: string;
  goal: string;
  /// Optional standing instructions for ad-hoc tasks (project tasks inherit
  /// the project's own instructions instead).
  instructions: string;
  /// Per-task model override; empty = use the default model.
  modelOverride: string;
  status: TabStatus;
  taskId: string;
  feed: FeedEntry[];
  artifacts: Artifact[];
  selectedArtifact: Artifact | null;
  handledApprovals: Record<string, boolean>;
  error: string;
  notice: string;
  /// Messages typed while the task runs, delivered as follow-up turns when
  /// the current run finishes.
  queued: string[];
  /// Project the task runs in, by name — used to scope "always allow"
  /// permission rules on approval cards. Empty for ad-hoc tasks.
  projectName: string;
};

type Settings = {
  interfaceLayout: InterfaceLayout;
  hasOnboarded: boolean;
  reopenFolders: boolean;
  autoOpenArtifacts: boolean;
  defaultModel: string;
};

type DaemonInfo = {
  name: string;
  version: unknown;
  log_path: string;
};

type ProjectMount = {
  path: string;
  access: "read_only" | "read_write";
  added_at: string;
};

type Project = {
  id: string;
  name: string;
  icon: string;
  purpose: string;
  instructions: string;
  mounts: ProjectMount[];
  model_default: string | null;
  task_ids: string[];
  created_at: string;
  updated_at: string;
};

type ApprovalMode = "always_ask" | "auto_allow_readonly";

type AuthProvider = {
  id: string;
  display_name: string;
  menu_detail: string;
  state: "available" | "expired" | "not_configured";
  method_detail: string;
  credential_source_detail: string;
  health_summary: string;
  api_key_settable: boolean;
};

type McpServerInfo = {
  name: string;
  command: string;
  args?: string[];
  url: string;
  transport: string;
  enabled: boolean;
  shared: boolean;
  timeout_secs?: number | null;
};

type ScheduleEntryInfo = {
  id: string;
  project_id: string;
  goal: string;
  model: string | null;
  run_at_unix_secs: number;
  created_at_unix_secs: number;
  status: "pending" | "done" | "failed" | "cancelled";
  task_id: string | null;
  fired_at_unix_secs: number | null;
  error: string | null;
};

type ToolInfo = {
  name: string;
  description: string;
  read_only: boolean;
};

type UsageLimitInfo = {
  name: string;
  usage_percent: number;
  resets_at: string | null;
};

type UsageReport = {
  providers: {
    provider_name: string;
    limits: UsageLimitInfo[];
    hard_limit_reached: boolean;
    error: string | null;
    last_used_unix_secs: number | null;
  }[];
  claude_windows: {
    five_hour: number;
    five_hour_resets_at: string | null;
    seven_day: number;
    seven_day_resets_at: string | null;
    seven_day_opus: number | null;
    model_scoped: { model_name: string; utilization: number; resets_at: string | null }[];
    extra_usage_enabled: boolean;
    last_error: string | null;
  };
};

type TranscriptEntry = {
  role: "user" | "assistant";
  text: string;
  events: { type: string; tool?: string; text?: string; is_error?: boolean }[];
};

type TranscriptView = {
  taskId: string;
  goal: string;
};

/// Must match `cowork_rpc::PROTOCOL_VERSION`. The daemon bumps it on breaking
/// RPC changes; the GUI warns instead of failing mysteriously on a mismatch.
/// v4 is additive: `TaskDelta` streams the assistant's text live.
const PROTOCOL_VERSION = 4;

const HISTORY_KEY = "jcode-cowork-history-v1";
const SETTINGS_KEY = "cworkspace-settings-v1";
const FOLDERS_KEY = "cworkspace-folders-v1";

const DEFAULT_SETTINGS: Settings = {
  interfaceLayout: "software",
  hasOnboarded: false,
  reopenFolders: true,
  autoOpenArtifacts: true,
  defaultModel: "",
};

const IS_MAC = typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || "");

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function text(data: Record<string, unknown>, ...keys: string[]) {
  for (const key of keys) {
    const value = data[key];
    if (typeof value === "string" && value.trim()) return value;
  }
  return "";
}

function identifier(data: Record<string, unknown>, key: string) {
  const value = data[key];
  return typeof value === "string" || typeof value === "number" ? String(value) : "";
}

function folderName(path: string) {
  return path.split(/[\\/]/).filter(Boolean).at(-1) || path;
}

function tabTitle(goal: string) {
  const trimmed = goal.trim();
  if (!trimmed) return "New session";
  const words = trimmed.split(/\s+/).slice(0, 7).join(" ");
  return words.length > 42 ? `${words.slice(0, 42)}…` : words;
}

/// Plain-language phrases for tool activity in Knowledge Work mode.
const TOOL_PHRASES: Record<string, string> = {
  read: "Looking through your files",
  ls: "Listing a folder",
  glob: "Finding files",
  agentgrep: "Searching file contents",
  webfetch: "Opening a web page",
  websearch: "Searching the web",
  write: "Writing files",
  edit: "Editing files",
  replace: "Editing files",
  apply_patch: "Applying changes",
  bash: "Running a command",
  office_docs: "Creating a document",
  browser: "Using the browser",
  open: "Opening a file",
  computer_use: "Controlling the computer",
  macos_computer_use: "Controlling the computer",
  compile_remote: "Compiling remotely",
  bg: "Running a background job",
  todo: "Updating its plan",
  memory: "Remembering details",
  session_search: "Searching past sessions",
  schedule: "Scheduling a follow-up",
  gmail: "Checking Gmail",
  calendar: "Checking Calendar",
  swarm: "Coordinating with helper agents",
  batch: "Running a batch of steps",
  skill_manage: "Managing its skills",
  conversation_search: "Searching past conversations",
  codemode: "Working through code",
  side_panel: "Opening the side panel",
  panel: "Opening the side panel",
  mcp: "Using an extension tool",
};

function toolPhrase(name: string) {
  if (name.startsWith("mcp__")) {
    const server = name.slice(5).split("__")[0] || "connected";
    return `Using the ${server} extension`;
  }
  return TOOL_PHRASES[name] || `Using ${name}`;
}

let tabCounter = 0;
function newTab(): SessionTab {
  tabCounter += 1;
  return {
    id: `tab-${tabCounter}-${Date.now().toString(16)}`,
    title: "New session",
    goal: "",
    instructions: "",
    modelOverride: "",
    status: "idle",
    taskId: "",
    feed: [],
    artifacts: [],
    selectedArtifact: null,
    handledApprovals: {},
    error: "",
    notice: "",
    queued: [],
    projectName: "",
  };
}

function initialHistory(): HistoryEntry[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(HISTORY_KEY) || "[]");
    return Array.isArray(value) ? (value as HistoryEntry[]) : [];
  } catch {
    return [];
  }
}

function loadSettings(): Settings {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}");
    if (value && typeof value === "object") {
      const raw = value as Record<string, unknown>;
      return {
        interfaceLayout: raw.interfaceLayout === "knowledge" || raw.interfaceLayout === "software"
          ? raw.interfaceLayout
          : DEFAULT_SETTINGS.interfaceLayout,
        hasOnboarded: typeof raw.hasOnboarded === "boolean" ? raw.hasOnboarded : DEFAULT_SETTINGS.hasOnboarded,
        reopenFolders: typeof raw.reopenFolders === "boolean" ? raw.reopenFolders : DEFAULT_SETTINGS.reopenFolders,
        autoOpenArtifacts: typeof raw.autoOpenArtifacts === "boolean" ? raw.autoOpenArtifacts : DEFAULT_SETTINGS.autoOpenArtifacts,
        defaultModel: typeof raw.defaultModel === "string" ? raw.defaultModel : DEFAULT_SETTINGS.defaultModel,
      };
    }
  } catch {
    // fall through to defaults
  }
  return { ...DEFAULT_SETTINGS };
}

function loadStoredFolders(): string[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(FOLDERS_KEY) || "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  } catch {
    return [];
  }
}

function decodeBase64(content: string) {
  const binary = atob(content);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes.buffer;
}

function App() {
  const [tabs, setTabs] = useState<SessionTab[]>(() => [newTab()]);
  const [activeTabId, setActiveTabId] = useState<string>(() => "");
  const [folders, setFolders] = useState<string[]>([]);
  const [history, setHistory] = useState<HistoryEntry[]>(initialHistory);
  const [daemonStatus, setDaemonStatus] = useState<"connecting" | "connected" | "offline">("connecting");
  const [previewOpen, setPreviewOpen] = useState(true);
  const [busy, setBusy] = useState(false);
  const [settings, setSettings] = useState<Settings>(loadSettings);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [daemonInfo, setDaemonInfo] = useState<DaemonInfo | null>(null);
  const [models, setModels] = useState<ModelRoute[]>([]);
  const [projects, setProjects] = useState<Project[]>([]);
  const [activeProjectId, setActiveProjectId] = useState("");
  const [projectDialogOpen, setProjectDialogOpen] = useState(false);
  /// Project being edited (null = create mode).
  const [editingProject, setEditingProject] = useState<Project | null>(null);
  const [scheduleEntries, setScheduleEntries] = useState<ScheduleEntryInfo[]>([]);
  const [scheduleDialogOpen, setScheduleDialogOpen] = useState(false);
  const [transcriptView, setTranscriptView] = useState<TranscriptView | null>(null);
  const [protocolWarning, setProtocolWarning] = useState("");
  const [reattachNotice, setReattachNotice] = useState("");
  const [editingTitle, setEditingTitle] = useState<{ tabId: string; value: string } | null>(null);
  const goalInputRef = useRef<HTMLTextAreaElement>(null);

  const activeProject = useMemo(
    () => projects.find((project) => project.id === activeProjectId) ?? null,
    [projects, activeProjectId],
  );

  function refreshProjects() {
    invoke<{ projects?: Project[] }>("project_list")
      .then((result) => setProjects(Array.isArray(result?.projects) ? result.projects : []))
      .catch(() => setProjects([]));
  }

  function refreshSchedule() {
    invoke<{ entries?: ScheduleEntryInfo[] }>("schedule_list")
      .then((result) => setScheduleEntries(Array.isArray(result?.entries) ? result.entries : []))
      .catch(() => setScheduleEntries([]));
  }

  // Scheduled tasks live daemon-side per project; refresh when the project
  // changes, the daemon reconnects, and after create/cancel flows.
  useEffect(() => {
    if (daemonStatus === "connected" && activeProject) refreshSchedule();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeProject?.id, daemonStatus]);

  const activeTab = useMemo(
    () => tabs.find((tab) => tab.id === activeTabId) ?? tabs[0],
    [tabs, activeTabId],
  );

  useEffect(() => {
    if (!tabs.some((tab) => tab.id === activeTabId) && tabs.length) {
      setActiveTabId(tabs[0].id);
    }
  }, [tabs, activeTabId]);

  useEffect(() => {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(history.slice(0, 30)));
  }, [history]);

  useEffect(() => {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
  }, [settings]);

  useEffect(() => {
    localStorage.setItem(FOLDERS_KEY, JSON.stringify(folders.slice(0, 12)));
  }, [folders]);

  // Connect once, then remount the folders from the previous run when the
  // user asked us to reopen them (Settings → Workspace).
  useEffect(() => {
    let cancelled = false;
    invoke<{ adopted?: boolean; running_tasks?: number }>("connect_daemon")
      .then(async (handshake) => {
        if (cancelled) return;
        setDaemonStatus("connected");
        // The daemon survives GUI close since v0.1.7: if this session
        // attached to an existing daemon with tasks still running, say so.
        if (handshake.adopted && (handshake.running_tasks || 0) > 0) {
          setReattachNotice(
            `Reattached to the background daemon — ${handshake.running_tasks} task(s) still running. Your work never stopped.`,
          );
        }
        // Protocol handshake (Milestone 0 exit criterion): a daemon whose
        // protocol version predates the GUI's would fail every newer RPC with
        // "Method not found" — surface that immediately instead.
        invoke<DaemonInfo>("daemon_info")
          .then((info) => {
            setDaemonInfo(info);
            const version = typeof info?.version === "number" ? info.version : 0;
            if (version && version < PROTOCOL_VERSION) {
              setProtocolWarning(
                `The daemon is running protocol v${version}, this app expects v${PROTOCOL_VERSION}. Restart the app or reinstall to update jcode.exe.`,
              );
            }
          })
          .catch(() => undefined);
        refreshProjects();
        const stored = loadStoredFolders();
        const { reopenFolders } = loadSettings();
        if (reopenFolders && stored.length) {
          const remounted: string[] = [];
          for (const path of stored) {
            try {
              const mounted = await invoke<string>("mount_folder", { path });
              if (!remounted.includes(mounted)) remounted.push(mounted);
            } catch {
              // Folder may have moved or been deleted since the last run;
              // skip it instead of failing the whole launch.
            }
          }
          if (!cancelled && remounted.length) setFolders(remounted);
        }
      })
      .catch((reason: unknown) => {
        if (!cancelled) {
          setDaemonStatus("offline");
          setTabs((current) => current.map((tab) => ({
            ...tab,
            error: tab.id === (activeTabId || current[0]?.id) ? `Could not connect to the Cworkspace daemon: ${errorMessage(reason)}` : tab.error,
          })));
        }
      });
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Model catalogs auto-fetch in the daemon; refresh whenever the
  // connection state becomes (or returns to) connected.
  useEffect(() => {
    if (daemonStatus !== "connected") return;
    let cancelled = false;
    invoke<{ models?: ModelRoute[] }>("list_models")
      .then((result) => {
        if (!cancelled) setModels(Array.isArray(result?.models) ? result.models : []);
      })
      .catch(() => {
        if (!cancelled) setModels([]);
      });
    return () => { cancelled = true; };
  }, [daemonStatus]);

  function patchTab(tabId: string, patch: Partial<SessionTab> | ((tab: SessionTab) => Partial<SessionTab>)) {
    setTabs((current) => current.map((tab) => tab.id === tabId
      ? { ...tab, ...(typeof patch === "function" ? patch(tab) : patch) }
      : tab));
  }

  function createTab() {
    const tab = newTab();
    setTabs((current) => [...current, tab]);
    setActiveTabId(tab.id);
    requestAnimationFrame(() => goalInputRef.current?.focus());
  }

  function closeTab(tabId: string) {
    setTabs((current) => {
      const target = current.find((tab) => tab.id === tabId);
      // A task left running has no window left to report into — stop it.
      if (target?.status === "running" && target.taskId) {
        invoke("stop_task", { taskId: target.taskId }).catch(() => undefined);
      }
      const next = current.filter((tab) => tab.id !== tabId);
      return next.length ? next : [newTab()];
    });
  }

  // Tab shortcuts: Ctrl/⌘+T opens a tab, Ctrl/⌘+N keeps its original
  // "new task" meaning (also a new tab), Ctrl/⌘+W closes the active one.
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (!(event.ctrlKey || event.metaKey)) return;
      const key = event.key.toLowerCase();
      if (key === "t" || key === "n") {
        event.preventDefault();
        createTab();
      } else if (key === "w") {
        event.preventDefault();
        if (activeTabId) closeTab(activeTabId);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTabId]);

  // The daemon tags every cowork event with the task that produced it
  // (task_id), so each event lands in the tab that owns the task — parallel
  // sessions never cross-contaminate. Events without a task id (older
  // daemons) fall back to the one running tab, preserving v0.1.1 behavior.
  const tabsRef = useRef<SessionTab[]>([]);
  useEffect(() => {
    tabsRef.current = tabs;
  }, [tabs]);

  // Streaming narrative (protocol v4): TaskDelta chunks arrive per token, so
  // they are batched into a ~120 ms flush — the feed types out live without
  // a React update for every single token. Chunks merge into the tab's last
  // narrative entry; a `replace` chunk starts a fresh one with the new text.
  const deltaBuffers = useRef<Map<string, { text: string; replace: boolean; timer: number | null }>>(new Map());
  const flushDelta = (tabId: string) => {
    const buffered = deltaBuffers.current.get(tabId);
    if (!buffered) return;
    deltaBuffers.current.delete(tabId);
    if (!buffered.text) return;
    const chunk = buffered.text;
    const replace = buffered.replace;
    setTabs((current) => current.map((tab) => {
      if (tab.id !== tabId) return tab;
      const last = tab.feed[tab.feed.length - 1];
      if (!replace && last && last.event === "TaskDelta") {
        const merged: FeedEntry = {
          ...last,
          data: { ...last.data, text: `${String(last.data.text ?? "")}${chunk}` },
        };
        return { ...tab, feed: [...tab.feed.slice(0, -1), merged] };
      }
      const entry: FeedEntry = {
        key: `delta-${Date.now()}-${Math.random().toString(16).slice(2)}`,
        event: "TaskDelta",
        data: { text: chunk },
        createdAt: Date.now(),
      };
      return { ...tab, feed: [...tab.feed, entry] };
    }));
  };
  const queueDelta = (tabId: string, chunk: string, replace: boolean) => {
    const existing = deltaBuffers.current.get(tabId);
    if (existing) {
      existing.text = replace ? chunk : existing.text + chunk;
      existing.replace = replace;
      return;
    }
    deltaBuffers.current.set(tabId, {
      text: chunk,
      replace,
      timer: window.setTimeout(() => flushDelta(tabId), 120),
    });
  };
  useEffect(() => () => {
    for (const buffered of deltaBuffers.current.values()) {
      if (buffered.timer !== null) window.clearTimeout(buffered.timer);
    }
    deltaBuffers.current.clear();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<CoworkEvent>("cowork-event", ({ payload }) => {
      // Streaming deltas take the buffered path; everything else flows
      // through the immutable per-event routing below.
      if (payload.event === "TaskDelta") {
        const data = payload.data || {};
        const chunk = String(data.text ?? "");
        if (!chunk) return;
        const tid = identifier(data, "task_id");
        const current = tabsRef.current;
        let targetId = "";
        if (tid) {
          const owner = current.find((tab) => tab.taskId === tid);
          if (owner) targetId = owner.id;
        } else {
          const running = current.filter((tab) => tab.status === "running");
          if (running.length === 1) targetId = running[0].id;
        }
        if (targetId) queueDelta(targetId, chunk, data.replace === true);
        return;
      }
      const entry: FeedEntry = {
        key: `${Date.now()}-${Math.random().toString(16).slice(2)}`,
        event: payload.event,
        data: payload.data || {},
        createdAt: Date.now(),
      };
      const taskId = identifier(entry.data, "task_id");
      // A completion flushes any still-buffered narrative first, so the
      // final text lands above the completion entry in order.
      if (entry.event === "TaskCompleted") {
        const current = tabsRef.current;
        const owner = taskId
          ? current.find((tab) => tab.taskId === taskId)
          : current.find((tab) => tab.status === "running");
        if (owner) flushDelta(owner.id);
      }
      setTabs((current) => {
        let targetId = "";
        if (taskId) {
          const owner = current.find((tab) => tab.taskId === taskId);
          if (owner) targetId = owner.id;
        } else {
          const running = current.filter((tab) => tab.status === "running");
          if (running.length === 1) targetId = running[0].id;
        }
        if (!targetId) return current;
        return current.map((tab) => {
          if (tab.id !== targetId) return tab;
          const next: Partial<SessionTab> = { feed: [...tab.feed, entry] };
          if (entry.event === "ArtifactCreated") {
            const path = text(entry.data, "path");
            if (path) {
              const artifact = { path, mime_type: text(entry.data, "mime_type") };
              next.artifacts = [artifact, ...tab.artifacts.filter((item) => item.path !== path)];
              next.selectedArtifact = artifact;
            }
          }
          if (entry.event === "TaskCompleted") {
            const success = entry.data.success !== false;
            next.status = success ? "completed" : "failed";
          }
          return { ...tab, ...next };
        });
      });
      if (entry.event === "TaskCompleted") {
        const doneTaskId = taskId || identifier(entry.data, "task_id");
        const success = entry.data.success !== false;
        if (doneTaskId) {
          setHistory((current) => current.map((item) => item.taskId === doneTaskId
            ? { ...item, status: success ? "completed" as const : "failed" as const, updatedAt: Date.now() }
            : item));
        }
      }
      if (entry.event === "ArtifactCreated") {
        const path = text(entry.data, "path");
        if (path) {
          setHistory((current) => current.map((item) => item.taskId === taskId
            ? { ...item, artifacts: [path, ...(item.artifacts || []).filter((existing) => existing !== path)] }
            : item));
        }
      }
      if (entry.event === "TaskCompleted" || entry.event === "ArtifactCreated") {
        if (loadSettings().autoOpenArtifacts && entry.event === "ArtifactCreated") setPreviewOpen(true);
      }
    })
      .then((stopListening) => {
        if (cancelled) stopListening();
        else unlisten = stopListening;
      })
      .catch((reason: unknown) => {
        if (!cancelled) {
          setTabs((current) => current.map((tab) => tab.id === (activeTabId || current[0]?.id)
            ? { ...tab, error: `Desktop event bridge unavailable: ${errorMessage(reason)}` }
            : tab));
        }
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTabId]);

  const pendingApprovals = useMemo(() => {
    const pending: { tabId: string; entry: FeedEntry }[] = [];
    for (const tab of tabs) {
      for (const entry of tab.feed) {
        if (entry.event === "ApprovalRequired" && !tab.handledApprovals[identifier(entry.data, "id")]) {
          pending.push({ tabId: tab.id, entry });
        }
      }
    }
    return pending;
  }, [tabs]);

  async function mountFolder() {
    const activeId = activeTab?.id;
    try {
      const selected = await open({ directory: true, multiple: false, title: "Choose a workspace folder" });
      if (typeof selected !== "string") return;
      setBusy(true);
      const mountedPath = await invoke<string>("mount_folder", { path: selected });
      setFolders((current) => current.includes(mountedPath) ? current : [...current, mountedPath]);
      setDaemonStatus("connected");
      if (activeId) patchTab(activeId, { error: "" });
    } catch (reason) {
      if (activeId) patchTab(activeId, { error: `Could not mount folder: ${errorMessage(reason)}` });
    } finally {
      setBusy(false);
    }
  }

  async function unmountFolder(path: string) {
    try {
      await invoke("unmount_folder", { path });
      setFolders((current) => current.filter((folder) => folder !== path));
    } catch (reason) {
      if (activeTab) patchTab(activeTab.id, { error: `Could not unmount folder: ${errorMessage(reason)}` });
    }
  }

  // Project-scoped mounts carry an explicit access level; the daemon rejects
  // structured writes into read-only folders before they touch disk.
  async function mountProjectFolder(access: "read_only" | "read_write") {
    if (!activeProject) return;
    try {
      const selected = await open({ directory: true, multiple: false, title: "Choose a folder for this project" });
      if (typeof selected !== "string") return;
      setBusy(true);
      await invoke("mount_add", { projectId: activeProject.id, path: selected, access });
      refreshProjects();
      setDaemonStatus("connected");
      if (activeTab) patchTab(activeTab.id, { error: "" });
    } catch (reason) {
      if (activeTab) patchTab(activeTab.id, { error: `Could not mount folder: ${errorMessage(reason)}` });
    } finally {
      setBusy(false);
    }
  }

  async function revokeProjectMount(path: string) {
    if (!activeProject) return;
    try {
      await invoke("mount_revoke", { projectId: activeProject.id, path });
      refreshProjects();
    } catch (reason) {
      if (activeTab) patchTab(activeTab.id, { error: `Could not revoke folder: ${errorMessage(reason)}` });
    }
  }

  async function createProject(name: string, purpose: string, instructions: string) {
    const result = await invoke<{ project?: Project }>("project_create", {
      name,
      purpose,
      instructions,
    });
    refreshProjects();
    if (result?.project?.id) setActiveProjectId(result.project.id);
    setProjectDialogOpen(false);
  }

  /// Persist edits from the project dialog (edit mode), including the
  /// per-project default model the daemon already supports.
  async function saveProjectEdits(projectId: string, values: {
    name: string;
    purpose: string;
    instructions: string;
    model_default: string;
  }) {
    await invoke("project_update", {
      projectId,
      name: values.name,
      purpose: values.purpose,
      instructions: values.instructions,
      modelDefault: values.model_default || undefined,
    });
    refreshProjects();
    setProjectDialogOpen(false);
    setEditingProject(null);
  }

  async function deleteProject(project: Project) {
    const confirmed = window.confirm(
      `Delete project “${project.name}”? Its folders, instructions, and task history are removed (files on disk are untouched).`,
    );
    if (!confirmed) return;
    try {
      await invoke("project_delete", { projectId: project.id });
      if (activeProjectId === project.id) setActiveProjectId("");
      refreshProjects();
    } catch (reason) {
      if (activeTab) patchTab(activeTab.id, { error: `Could not delete project: ${errorMessage(reason)}` });
    }
  }

  async function scheduleTask(projectId: string, goal: string, runAtUnixSecs: number, model?: string) {
    await invoke("schedule_create", { projectId, goal, runAtUnixSecs, model: model || undefined });
    setScheduleDialogOpen(false);
    refreshSchedule();
  }

  async function cancelSchedule(id: string) {
    try {
      await invoke("schedule_cancel", { id });
      refreshSchedule();
    } catch (reason) {
      if (activeTab) patchTab(activeTab.id, { error: `Could not cancel the scheduled task: ${errorMessage(reason)}` });
    }
  }

  async function startTask(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const tab = activeTab;
    if (!tab) return;
    const goal = tab.goal.trim();
    if (!goal) return;
    // While a task runs, the composer delivers queued messages instead of
    // starting anything new.
    if (tab.status === "running") {
      patchTab(tab.id, { queued: [...tab.queued, goal], goal: "" });
      return;
    }
    patchTab(tab.id, { error: "", notice: "" });
    if (activeProject && !activeProject.mounts.length) {
      patchTab(tab.id, { error: "Mount a folder into this project before starting a task." });
      return;
    }
    if (!activeProject && !folders.length) {
      patchTab(tab.id, { error: "Mount a workspace folder before starting a task." });
      return;
    }
    setBusy(true);
    patchTab(tab.id, {
      feed: [],
      artifacts: [],
      selectedArtifact: null,
      handledApprovals: {},
      queued: [],
      projectName: activeProject?.name || "",
      title: tabTitle(goal),
    });
    try {
      // Model precedence: this tab's override → the app default → the
      // project's default (applied daemon-side) → the provider default.
      const model = tab.modelOverride || settings.defaultModel || undefined;
      const taskId = await invoke<string>("start_cowork_task", {
        goal,
        projectId: activeProject ? activeProject.id : undefined,
        allowedRoots: activeProject ? [] : folders,
        model,
        instructions: activeProject ? undefined : tab.instructions || undefined,
      });
      patchTab(tab.id, {
        taskId,
        status: "running",
        title: tabTitle(goal),
      });
      setHistory((current) => [{
        taskId,
        goal,
        status: "running" as const,
        updatedAt: Date.now(),
        model: model || undefined,
      }, ...current.filter((item) => item.taskId !== taskId)].slice(0, 30));
    } catch (reason) {
      patchTab(tab.id, { error: `Could not start task: ${errorMessage(reason)}` });
    } finally {
      setBusy(false);
    }
  }

  async function stopTask() {
    const tab = activeTab;
    if (!tab?.taskId) return;
    patchTab(tab.id, { error: "" });
    try {
      await invoke("stop_task", { taskId: tab.taskId });
      patchTab(tab.id, { status: "stopped" });
      setHistory((current) => current.map((item) => item.taskId === tab.taskId ? { ...item, status: "stopped", updatedAt: Date.now() } : item));
    } catch (reason) {
      patchTab(tab.id, { error: `Could not stop task: ${errorMessage(reason)}` });
    }
  }

  async function answerApproval(tabId: string, id: string | number, approved: boolean, remember?: "global" | "project") {
    try {
      await invoke("respond_approval", { id, approved, remember: remember || undefined });
      setTabs((current) => current.map((tab) => tab.id === tabId
        ? { ...tab, handledApprovals: { ...tab.handledApprovals, [id]: approved } }
        : tab));
    } catch (reason) {
      patchTab(tabId, { error: `Could not send approval response: ${errorMessage(reason)}` });
    }
  }

  function restoreHistory(item: HistoryEntry) {
    const tab = activeTab;
    if (!tab) return;
    const restoredArtifacts = (item.artifacts || []).map((path) => ({ path, mime_type: "" }));
    patchTab(tab.id, {
      goal: item.goal,
      title: tabTitle(item.goal),
      taskId: "",
      status: "idle",
      feed: [],
      artifacts: restoredArtifacts,
      selectedArtifact: restoredArtifacts[0] || null,
      error: "",
      notice: `Loaded “${item.goal}” into the composer — review it, then start a fresh task.${restoredArtifacts.length ? " Deliverables from the original run are listed in Artifacts." : ""}`,
    });
    if (restoredArtifacts.length) setPreviewOpen(true);
    requestAnimationFrame(() => goalInputRef.current?.focus());
  }

  function openSettings() {
    setSettingsOpen(true);
    if (daemonStatus === "connected") {
      invoke<DaemonInfo>("daemon_info")
        .then((info) => setDaemonInfo(info))
        .catch(() => setDaemonInfo(null));
    }
  }

  function updateSetting<K extends keyof Settings>(key: K, value: Settings[K]) {
    setSettings((current) => ({ ...current, [key]: value }));
  }

  const availableModels = models.filter((route) => route.available);
  const isKnowledge = settings.interfaceLayout === "knowledge";

  function removeQueued(index: number) {
    const tab = activeTab;
    if (!tab) return;
    patchTab(tab.id, { queued: tab.queued.filter((_, at) => at !== index) });
  }

  /// Deliver the first queued message as a follow-up turn on the finished
  /// task — the daemon resumes its session under the same task id, so the
  /// feed and transcript continue in this tab.
  async function sendFollowUp(index: number) {
    const tab = activeTab;
    if (!tab?.taskId) return;
    const message = tab.queued[index];
    if (!message?.trim()) return;
    patchTab(tab.id, { error: "", status: "running", queued: tab.queued.filter((_, at) => at !== index) });
    try {
      await invoke("task_followup", {
        taskId: tab.taskId,
        message: message.trim(),
        model: tab.modelOverride || undefined,
      });
      setHistory((current) => current.map((item) => item.taskId === tab.taskId ? { ...item, status: "running" as const, updatedAt: Date.now() } : item));
    } catch (reason) {
      patchTab(tab.id, { error: `Could not send the follow-up: ${errorMessage(reason)}`, status: "failed" });
    }
  }

  return (
  <ErrorBoundary>
    <main className="app-shell">
      <aside className="sidebar">
        <div className="brand-lockup">
          <div className="brand-mark"><span>C</span><i /></div>
          <div><div className="brand-name">C<span>workspace</span></div><div className="brand-subtitle">A space for the work ahead</div></div>
        </div>

        <button className="new-session" onClick={createTab}>
          <Plus size={16} strokeWidth={2.2} /> New task <span>{IS_MAC ? "⌘ T" : "Ctrl T"}</span>
        </button>

        <section className="sidebar-section">
          <div className="section-heading"><span>PROJECTS</span><button className="icon-button small" aria-label="New project" title="New project" onClick={() => setProjectDialogOpen(true)}><Plus size={15} /></button></div>
          <div className="project-list">
            <button className={`project-item ${!activeProjectId ? "selected" : ""}`} onClick={() => setActiveProjectId("")} title="Ad-hoc tasks using session-mounted folders">
              <span className="project-icon"><Sparkles size={14} /></span><span className="project-name">Ad-hoc sessions</span>
            </button>
            {projects.map((project) => (
              <button
                className={`project-item ${activeProjectId === project.id ? "selected" : ""}`}
                key={project.id}
                onClick={() => setActiveProjectId(project.id)}
                title={project.purpose || project.name}
              >
                <span className="project-icon"><Folder size={14} /></span>
                <span className="project-name">{project.name}</span>
                <small className="project-count">{project.task_ids.length}</small>
              </button>
            ))}
            {!projects.length && <p className="sidebar-empty">No projects yet. <button className="link-button" onClick={() => setProjectDialogOpen(true)}>Create one</button> to keep files, instructions, and history together.</p>}
          </div>
        </section>

        {activeProject ? (
          <section className="sidebar-section">
            <div className="section-heading">
              <span>PROJECT FOLDERS</span>
              <span className="heading-actions">
                <button className="icon-button small" aria-label="Edit project" title="Edit project (name, purpose, instructions, default model)" onClick={() => { setEditingProject(activeProject); setProjectDialogOpen(true); }}><Pencil size={14} /></button>
                <button className="icon-button small" aria-label="Delete project" title="Delete project" onClick={() => deleteProject(activeProject)}><Trash2 size={14} /></button>
              </span>
            </div>
            <div className="mount-choices">
              <button className="mount-choice" onClick={() => mountProjectFolder("read_write")} disabled={busy}><Folder size={15} /><span>Mount folder — read &amp; write</span></button>
              <button className="mount-choice" onClick={() => mountProjectFolder("read_only")} disabled={busy}><Folder size={15} /><span>Mount folder — read only</span></button>
            </div>
            <div className="folder-list">
              {activeProject.mounts.map((mount) => (
                <div className="folder-item" key={mount.path} title={mount.path}>
                  <span className={`folder-dot ${mount.access === "read_only" ? "readonly" : ""}`} />
                  <span className="folder-label">{folderName(mount.path)}</span>
                  <span className={`access-badge ${mount.access}`}>{mount.access === "read_only" ? "read-only" : "read/write"}</span>
                  <button className="icon-button folder-remove" aria-label={`Revoke ${folderName(mount.path)}`} title="Revoke access" onClick={() => revokeProjectMount(mount.path)}><X size={14} /></button>
                </div>
              ))}
              {!activeProject.mounts.length && <p className="sidebar-empty">Mount a folder to give this project a workspace.</p>}
            </div>
            <p className="sidebar-hint">Tasks in “{activeProject.name}” run only inside these folders — writes stay out of read-only mounts.</p>
            <div className="section-heading schedule-heading"><span>SCHEDULED</span><button className="icon-button small" aria-label="Schedule a task" title="Schedule a task for this project" onClick={() => setScheduleDialogOpen(true)}><CalendarClock size={14} /></button></div>
            <div className="schedule-list">
              {scheduleEntries.filter((entry) => entry.project_id === activeProject.id && entry.status !== "cancelled").map((entry) => (
                <div className="schedule-item" key={entry.id} title={entry.error || entry.goal}>
                  <span className={`schedule-dot ${entry.status}`} />
                  <span className="schedule-copy">
                    <strong>{entry.goal}</strong>
                    <small>{entry.status === "pending"
                      ? `runs ${new Date(entry.run_at_unix_secs * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })}`
                      : entry.status === "done"
                        ? `ran · task ${entry.task_id?.slice(0, 14) || ""}`
                        : `failed — ${entry.error || "unknown error"}`}</small>
                  </span>
                  {entry.status === "pending" && <button className="icon-button folder-remove" aria-label="Cancel scheduled task" title="Cancel" onClick={() => cancelSchedule(entry.id)}><X size={13} /></button>}
                </div>
              ))}
              {!scheduleEntries.some((entry) => entry.project_id === activeProject.id && entry.status !== "cancelled") && <p className="sidebar-empty">No scheduled tasks. Use the clock icon to run a goal later.</p>}
            </div>
          </section>
        ) : (
          <section className="sidebar-section">
            <div className="section-heading"><span>WORKSPACES</span><button className="icon-button small" aria-label="Mount folder" title="Mount folder" onClick={mountFolder}><FolderPlus size={15} /></button></div>
            <button className="mount-row" onClick={mountFolder} disabled={busy}><Folder size={16} /><span>Mount a folder</span><Plus size={14} /></button>
            <div className="folder-list">
              {folders.map((path) => (
                <div className="folder-item" key={path} title={path}>
                  <span className="folder-dot" />
                  <span className="folder-label">{folderName(path)}</span>
                  <button className="icon-button folder-remove" aria-label={`Unmount ${folderName(path)}`} title="Unmount folder" onClick={() => unmountFolder(path)}><X size={14} /></button>
                </div>
              ))}
              {!folders.length && <p className="sidebar-empty">Your folders stay yours. Mount one to begin.</p>}
            </div>
          </section>
        )}

        {!isKnowledge && <section className="sidebar-section models-section">
          <div className="section-heading"><span>MODELS</span><Sparkles size={14} /></div>
          <div className="model-list">
            {availableModels.slice(0, 10).map((route) => (
              <button
                className={`model-item ${settings.defaultModel === route.model ? "selected" : ""}`}
                key={`${route.provider}:${route.model}`}
                title={`${route.detail}\n${settings.defaultModel === route.model ? "Default for new tasks" : "Click to make default"}`}
                onClick={() => updateSetting("defaultModel", route.model)}
              >
                <span className="model-name">{route.model}</span>
                <span className="model-provider">{settings.defaultModel === route.model ? "default" : route.provider}</span>
              </button>
            ))}
            {!models.length && <p className="sidebar-empty">Model catalogs load automatically once signed in.</p>}
            {models.length > 0 && !availableModels.length && <p className="sidebar-empty">No models available yet — sign in to a provider to fill this list.</p>}
            {availableModels.length > 0 && <p className="sidebar-hint">Click a model to make it the default for new tasks.</p>}
          </div>
        </section>}

        <section className="sidebar-section history-section">
          <div className="section-heading"><span>RECENT WORK</span><History size={14} /></div>
          <div className="history-list">
            {history.map((item) => (
              <button className={`history-item ${activeTab?.taskId === item.taskId ? "selected" : ""}`} key={item.taskId} onClick={() => restoreHistory(item)} title={item.goal}>
                <span className={`history-status ${item.status}`} />
                <span className="history-copy">
                  <strong>{tabTitle(item.goal)}</strong>
                  <small>{new Date(item.updatedAt).toLocaleDateString(undefined, { month: "short", day: "numeric" })} · {item.status}{item.artifacts?.length ? ` · ${item.artifacts.length} file${item.artifacts.length === 1 ? "" : "s"}` : ""}</small>
                </span>
                <ChevronRight size={14} />
              </button>
            ))}
            {!history.length && <p className="sidebar-empty">Your finished tasks will gather here.</p>}
          </div>
        </section>

        <div className="sidebar-footer">
          <div className="avatar">J</div>
          <div><strong>Local workspace</strong><span>Private by default</span></div>
          <button className="icon-button" aria-label="Open settings" title="Settings" onClick={openSettings}><Settings size={16} /></button>
          <ShieldCheck size={16} />
        </div>
      </aside>

      <section className="work-area">
        <header className="topbar">
          <div className="tab-strip" role="tablist" aria-label="Sessions">
            {tabs.map((tab) => (
              <div
                key={tab.id}
                role="tab"
                aria-selected={tab.id === activeTab?.id}
                className={`session-tab ${tab.id === activeTab?.id ? "active" : ""}`}
                onClick={() => setActiveTabId(tab.id)}
                title={tab.title}
              >
                <span className={`tab-status ${tab.status} ${tab.status === "running" ? "pulse" : ""}`} />
                {editingTitle?.tabId === tab.id ? (
                  <input
                    className="tab-title-input"
                    value={editingTitle.value}
                    autoFocus
                    aria-label="Tab title"
                    onChange={(event) => setEditingTitle({ tabId: tab.id, value: event.target.value })}
                    onClick={(event) => event.stopPropagation()}
                    onBlur={() => { patchTab(tab.id, { title: editingTitle.value.trim() || tab.title }); setEditingTitle(null); }}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") { patchTab(tab.id, { title: editingTitle.value.trim() || tab.title }); setEditingTitle(null); }
                      if (event.key === "Escape") setEditingTitle(null);
                    }}
                  />
                ) : (
                  <span
                    className="tab-title"
                    onDoubleClick={(event) => { event.stopPropagation(); setEditingTitle({ tabId: tab.id, value: tab.title }); }}
                    title={`${tab.title} — double-click to rename`}
                  >{tab.title}</span>
                )}
                <button
                  className="icon-button tab-close"
                  aria-label={`Close ${tab.title}`}
                  onClick={(event) => { event.stopPropagation(); closeTab(tab.id); }}
                ><X size={12} /></button>
              </div>
            ))}
            <button className="icon-button tab-new" aria-label="New tab" title={IS_MAC ? "New tab (⌘T)" : "New tab (Ctrl+T)"} onClick={createTab}><Plus size={14} /></button>
          </div>
          <div className="connection-state"><span className={`status-light ${tabs.some((tab) => tab.status === "running") ? "working" : daemonStatus === "connected" ? "online" : ""}`} />{tabs.some((tab) => tab.status === "running") ? "Working" : daemonStatus === "connecting" ? "Connecting…" : daemonStatus === "offline" ? <button className="reconnect-button" onClick={() => { setDaemonStatus("connecting"); invoke("connect_daemon").then(() => setDaemonStatus("connected")).catch((reason: unknown) => { setDaemonStatus("offline"); if (activeTab) patchTab(activeTab.id, { error: `Could not reach the Cworkspace daemon: ${errorMessage(reason)}` }); }); }}>Reconnect</button> : "Connected"}</div>
        </header>

        <div className="center-scroll">
          <div className="welcome-block">
            <div className="eyebrow"><span className="eyebrow-line" /> YOUR DESK, REIMAGINED</div>
            <h1>Make room for<br /><em>good work.</em></h1>
            <p>Bring a goal. Cworkspace will organize the steps, keep you in the loop, and place every deliverable right here — one tab per task, in parallel.</p>
          </div>

          <form className="goal-form" onSubmit={startTask}>
            <div className="goal-form-top"><Sparkles size={16} /><label htmlFor="goal-input">{activeTab?.status === "running" ? (isKnowledge ? "Add a message for the agent — it runs after the current step" : "Queue a message for the running task") : "What would you like to get done?"}</label>{(activeProject ? activeProject.mounts.length : folders.length) > 0 && <span className="root-count">{activeProject ? `${activeProject.name} · ${activeProject.mounts.length} ${activeProject.mounts.length === 1 ? "folder" : "folders"}` : `${folders.length} ${folders.length === 1 ? "folder" : "folders"} in scope`}</span>}</div>
            <textarea id="goal-input" ref={goalInputRef} value={activeTab?.goal || ""} onChange={(event) => activeTab && patchTab(activeTab.id, { goal: event.target.value })} placeholder="Draft a project brief, audit these figures, prepare a polished report…" rows={3} />
            {!activeProject && !isKnowledge && <textarea
              id="instructions-input"
              className="composer-instructions"
              value={activeTab?.instructions || ""}
              onChange={(event) => activeTab && patchTab(activeTab.id, { instructions: event.target.value })}
              placeholder="Optional standing instructions for this session — tone, format, guardrails…"
              rows={1}
              disabled={activeTab?.status === "running"}
            />}
            {(activeTab?.queued.length || 0) > 0 && <div className="queue-strip">
              {activeTab?.queued.map((message, index) => (
                <span className="queue-chip" key={`${index}-${message.slice(0, 12)}`}>
                  <span className="queue-copy" title={message}>{message}</span>
                  {activeTab.status === "running"
                    ? <small>queued</small>
                    : <button type="button" className="queue-send" onClick={() => sendFollowUp(index)}>{index === 0 ? "Send as follow-up" : "Send"}</button>}
                  <button type="button" className="icon-button folder-remove" aria-label="Remove queued message" onClick={() => removeQueued(index)}><X size={12} /></button>
                </span>
              ))}
            </div>}
            <div className="goal-form-bottom">
              <div className="composer-context">
                {!isKnowledge && <select
                  className="settings-select model-override"
                  value={activeTab?.modelOverride || ""}
                  aria-label="Model for this task"
                  disabled={activeTab?.status === "running"}
                  onChange={(event) => activeTab && patchTab(activeTab.id, { modelOverride: event.target.value })}
                >
                  <option value="">{settings.defaultModel ? `Default · ${settings.defaultModel}` : "Provider default model"}</option>
                  {availableModels.map((route) => (
                    <option key={`${route.provider}:${route.model}`} value={route.model}>{route.model} · {route.provider}</option>
                  ))}
                </select>}
                <span><CircleHelp size={14} /> {activeTab?.status === "running"
                  ? "Your message joins the queue and runs as a follow-up when this step finishes."
                  : activeProject ? `Runs inside “${activeProject.name}” with its instructions and folders.` : isKnowledge ? "Your mounted folders define the workspace boundary." : activeTab?.instructions ? "Custom instructions will apply to this task." : "Your mounted folders define the workspace boundary."}</span>
              </div>
              <button className="primary-action" type="submit" disabled={!activeTab?.goal.trim() || busy}>{busy ? <LoaderCircle className="spin" size={16} /> : activeTab?.status === "running" ? null : <Play size={15} fill="currentColor" />} {activeTab?.status === "running" ? "Queue message" : isKnowledge ? "Start" : "Start task"} <ArrowUpRight size={15} /></button>
            </div>
          </form>

          {protocolWarning && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{protocolWarning}</span><button className="icon-button" aria-label="Dismiss warning" onClick={() => setProtocolWarning("")}><X size={15} /></button></div>}
          {reattachNotice && <div className="info-banner" role="status"><Sparkles size={15} /><span>{reattachNotice}</span><button className="icon-button" aria-label="Dismiss notice" onClick={() => setReattachNotice("")}><X size={15} /></button></div>}
          {activeTab?.notice && <div className="info-banner" role="status"><Sparkles size={15} /><span>{activeTab.notice}</span><button className="icon-button" aria-label="Dismiss notice" onClick={() => patchTab(activeTab.id, { notice: "" })}><X size={14} /></button></div>}
          {activeTab?.error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{activeTab.error}</span><button className="icon-button" aria-label="Dismiss error" onClick={() => patchTab(activeTab.id, { error: "" })}><X size={15} /></button></div>}

          <div className="feed-heading"><div><span className="feed-kicker">LIVE SESSION</span><h2>{activeTab?.status === "running" ? "Work in motion" : activeTab?.status === "completed" ? "Session complete" : activeTab?.status === "stopped" || activeTab?.status === "failed" ? "Session stopped" : "Session"}</h2></div><div className="feed-actions">{activeTab?.status === "running" && <button className="stop-action" onClick={stopTask}><StopCircle size={15} /> Stop task</button>}{activeTab && activeTab.status !== "running" && activeTab.status !== "idle" && activeTab.taskId && <button className="transcript-action" onClick={() => setTranscriptView({ taskId: activeTab.taskId, goal: activeTab.title })}><FileText size={15} /> View transcript</button>}<span className={`task-badge ${activeTab?.status || "idle"}`}>{activeTab?.status === "running" ? <><i className="pulse-dot" /> In progress</> : activeTab?.status === "completed" ? "Completed" : activeTab?.status === "stopped" ? "Stopped" : activeTab?.status === "failed" ? "Failed" : "Waiting"}</span></div></div>

          <div className="feed-list">
            {activeTab?.feed.map((entry) => <FeedCard key={entry.key} entry={entry} handled={activeTab.handledApprovals} plain={isKnowledge} projectName={activeTab.projectName} onAnswer={(id, approved, remember) => answerApproval(activeTab.id, id, approved, remember)} />)}
            {activeTab && !activeTab.feed.length && <div className="feed-empty"><div className="empty-orbit"><ArrowDown size={19} /></div><strong>{activeTab.status === "running" ? "Connecting to your task…" : "The next step starts with you"}</strong><span>When work begins, agents, tool activity, and approval requests will appear here.</span></div>}
          </div>
          {Boolean(activeTab?.feed.length) && <div className="feed-end"><span /> Session activity <span /></div>}
        </div>
      </section>

      <aside className={`artifact-panel ${previewOpen ? "open" : "closed"}`}>
        <div className="artifact-header"><div><span className="feed-kicker">DELIVERABLES</span><h2>Artifacts <span>{(activeTab?.artifacts.length || 0).toString().padStart(2, "0")}</span></h2></div><button className="icon-button" aria-label="Close artifact panel" title="Close panel" onClick={() => setPreviewOpen(false)}><PanelRightClose size={17} /></button></div>
        {!previewOpen ? <button className="reopen-preview" onClick={() => setPreviewOpen(true)}>Show artifacts <ChevronDown size={15} /></button> : (
          <>
            {Boolean(activeTab?.artifacts.length) && <div className="artifact-tabs">{activeTab?.artifacts.map((artifact) => <button key={artifact.path} className={`artifact-tab ${activeTab.selectedArtifact?.path === artifact.path ? "active" : ""}`} title={artifact.path} onClick={() => patchTab(activeTab.id, { selectedArtifact: artifact })}>{iconForArtifact(artifact.path)}<span>{folderName(artifact.path)}</span></button>)}</div>}
            <ArtifactPreview artifact={activeTab?.selectedArtifact || null} />
          </>
        )}
        <div className="artifact-footnote"><ShieldCheck size={14} /><span>Previews read only from mounted folders</span></div>
      </aside>

      {pendingApprovals.length > 0 && <button className="approval-counter" onClick={() => { setActiveTabId(pendingApprovals[0].tabId); }} title="Show the session that needs a decision"><span className="pulse-dot" /> {pendingApprovals.length} decision{pendingApprovals.length === 1 ? "" : "s"} needed</button>}

      {settingsOpen && <SettingsDialog
        settings={settings}
        models={models}
        daemonInfo={daemonInfo}
        daemonStatus={daemonStatus}
        onChange={updateSetting}
        onClose={() => setSettingsOpen(false)}
      />}

      {projectDialogOpen && <ProjectDialog
        project={editingProject}
        models={models}
        defaultModel={settings.defaultModel}
        onClose={() => { setProjectDialogOpen(false); setEditingProject(null); }}
        onCreate={createProject}
        onUpdate={saveProjectEdits}
      />}

      {scheduleDialogOpen && activeProject && <ScheduleDialog
        projectId={activeProject.id}
        projectName={activeProject.name}
        defaultModel={activeProject.model_default || settings.defaultModel}
        onClose={() => setScheduleDialogOpen(false)}
        onSchedule={scheduleTask}
      />}

      {transcriptView && <TranscriptDialog view={transcriptView} onClose={() => setTranscriptView(null)} />}

      {!settings.hasOnboarded && <OnboardingDialog settings={settings} onChange={updateSetting} onDone={() => {}} />}
    </main>
  </ErrorBoundary>);
}

function ProjectDialog({ project, models, defaultModel, onClose, onCreate, onUpdate }: {
  project?: Project | null;
  models: ModelRoute[];
  defaultModel: string;
  onClose: () => void;
  onCreate: (name: string, purpose: string, instructions: string) => Promise<void>;
  onUpdate: (projectId: string, values: { name: string; purpose: string; instructions: string; model_default: string }) => Promise<void>;
}) {
  const editing = Boolean(project);
  const [name, setName] = useState(project?.name || "");
  const [purpose, setPurpose] = useState(project?.purpose || "");
  const [instructions, setInstructions] = useState(project?.instructions || "");
  const [modelDefault, setModelDefault] = useState(project?.model_default || "");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!name.trim()) {
      setError("Give the project a name.");
      return;
    }
    setBusy(true);
    setError("");
    try {
      if (editing && project) {
        await onUpdate(project.id, {
          name: name.trim(),
          purpose: purpose.trim(),
          instructions,
          model_default: modelDefault,
        });
      } else {
        await onCreate(name.trim(), purpose.trim(), instructions);
      }
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="settings-overlay" onClick={onClose} role="presentation">
      <form className="settings-dialog" role="dialog" aria-modal="true" aria-label={editing ? "Edit project" : "New project"} onSubmit={submit} onClick={(event) => event.stopPropagation()}>
        <header className="settings-header">
          <div><span className="feed-kicker">{editing ? "EDIT PROJECT" : "NEW PROJECT"}</span><h2>{editing ? `Edit “${project?.name}”` : "Create a project"}</h2></div>
          <button type="button" className="icon-button" aria-label="Close project dialog" onClick={onClose}><X size={17} /></button>
        </header>
        <section className="settings-section">
          <h3>Identity</h3>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Name</strong><small>Shown in the sidebar; groups folders, instructions, and task history.</small></span>
            <input className="settings-input" value={name} autoFocus aria-label="Project name" placeholder="Q4 Close" onChange={(event) => setName(event.target.value)} />
          </div>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Purpose</strong><small>One line describing what this project is for.</small></span>
            <input className="settings-input" value={purpose} aria-label="Project purpose" placeholder="Finance workspace for the quarterly close" onChange={(event) => setPurpose(event.target.value)} />
          </div>
        </section>
        <section className="settings-section">
          <h3>Instructions</h3>
          <textarea
            className="settings-textarea"
            value={instructions}
            aria-label="Project instructions"
            placeholder="Standing guidance for every task in this project — tone, formats, source-of-truth folders, review rules…"
            rows={5}
            onChange={(event) => setInstructions(event.target.value)}
          />
          <p className="sidebar-hint">Injected into every task the project starts.</p>
        </section>
        <section className="settings-section">
          <h3>Default model</h3>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Model for project tasks</strong><small>Tasks in this project start on this model unless overridden in the composer.</small></span>
            <select className="settings-select" value={modelDefault} aria-label="Project default model" onChange={(event) => setModelDefault(event.target.value)}>
              <option value="">{defaultModel ? `App default · ${defaultModel}` : "Provider default"}</option>
              {models.filter((route) => route.available).map((route) => (
                <option key={`${route.provider}:${route.model}`} value={route.model}>{route.model} · {route.provider}</option>
              ))}
            </select>
          </div>
        </section>
        {error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{error}</span></div>}
        <footer className="settings-footer">
          <button type="button" className="secondary-action" onClick={onClose}>Cancel</button>
          <button type="submit" className="primary-action" disabled={busy || !name.trim()}>{busy ? <LoaderCircle className="spin" size={15} /> : editing ? <Check size={15} /> : <Plus size={15} />} {editing ? "Save changes" : "Create project"}</button>
        </footer>
      </form>
    </div>
  );
}

/// The Interface Layout choice, cloned from the reference design: two cards
/// for the simplified Knowledge Work face vs the full Software Engineering
/// interface. Shared by Settings → General and first-run onboarding.
function LayoutCards({ value, onSelect }: { value: InterfaceLayout; onSelect: (layout: InterfaceLayout) => void }) {
  return (
    <div className="layout-cards">
      <button type="button" className={`layout-card ${value === "knowledge" ? "selected" : ""}`} onClick={() => onSelect("knowledge")}>
        <Briefcase size={16} />
        <strong>Knowledge Work</strong>
        <small>Simplified interface for research, writing, and everyday tasks.</small>
      </button>
      <button type="button" className={`layout-card ${value === "software" ? "selected" : ""}`} onClick={() => onSelect("software")}>
        <Wrench size={16} />
        <strong>Software Engineering</strong>
        <small>Full interface for coding, developer tools, and managing agents.</small>
      </button>
    </div>
  );
}

/// First-run onboarding: pick a layout, connect a provider, done. Every step
/// reuses an existing surface; skipping is always available.
function OnboardingDialog({ settings, onChange, onDone }: {
  settings: Settings;
  onChange: <K extends keyof Settings>(key: K, value: Settings[K]) => void;
  onDone: () => void;
}) {
  const [step, setStep] = useState(1);
  function finish() {
    onChange("hasOnboarded", true);
    onDone();
  }
  return (
    <div className="settings-overlay" role="presentation">
      <div className="settings-dialog onboarding-dialog" role="dialog" aria-modal="true" aria-label="Welcome to Cworkspace">
        <header className="settings-header">
          <div><span className="feed-kicker">WELCOME</span><h2>{step === 1 ? "Choose your workspace" : "Connect your providers"}</h2></div>
          <div className="onboarding-dots"><i className={step === 1 ? "on" : ""} /><i className={step === 2 ? "on" : ""} /></div>
        </header>
        {step === 1 ? (
          <>
            <section className="settings-section">
              <p className="sidebar-hint">Pick the interface that fits your work — you can switch anytime in Settings → General.</p>
              <LayoutCards value={settings.interfaceLayout} onSelect={(layout) => onChange("interfaceLayout", layout)} />
            </section>
            <footer className="settings-footer">
              <button type="button" className="secondary-action" onClick={finish}>Skip setup</button>
              <button type="button" className="primary-action" onClick={() => setStep(2)}>Next <ChevronRight size={15} /></button>
            </footer>
          </>
        ) : (
          <>
            <section className="settings-section">
              <p className="sidebar-hint">Sign in to at least one provider — the same credentials the jcode CLI uses. You can add more later in Settings → Accounts.</p>
              <AccountsSection />
            </section>
            <footer className="settings-footer">
              <button type="button" className="secondary-action" onClick={() => setStep(1)}>Back</button>
              <button type="button" className="primary-action" onClick={finish}><Check size={15} /> Start working</button>
            </footer>
          </>
        )}
      </div>
    </div>
  );
}

/// Stored permission rule as returned by the daemon.
type RuleRecord = {
  id: string;
  tool: string;
  decision: "allow" | "deny";
  scope: { kind: "global" } | { kind: "project"; project_id: string };
  created_at: number;
};

/// Common tool names offered in the rule editor's datalist.
const RULE_TOOL_SUGGESTIONS = [
  "bash", "write", "edit", "apply_patch", "read", "ls", "glob", "agentgrep",
  "webfetch", "websearch", "browser", "open", "office_docs", "computer_use",
  "gmail", "calendar", "memory", "bg", "schedule",
];

/// Settings → Permissions: the stored allow/deny rules and the form to add
/// more. Rules are checked before the approval policy on every tool call, so
/// "always allow websearch" means exactly that — no more per-file prompts.
function PermissionRulesSection() {
  const [rules, setRules] = useState<RuleRecord[]>([]);
  const [projects, setProjects] = useState<Project[]>([]);
  const [tool, setTool] = useState("");
  const [decision, setDecision] = useState<"allow" | "deny">("allow");
  const [scope, setScope] = useState("global");
  const [error, setError] = useState("");
  const [loaded, setLoaded] = useState(false);

  async function refresh() {
    try {
      const [rulesResult, projectsResult] = await Promise.all([
        invoke<{ rules?: RuleRecord[] }>("permission_list"),
        invoke<{ projects?: Project[] }>("project_list"),
      ]);
      setRules(rulesResult.rules || []);
      setProjects(projectsResult.projects || []);
      setError("");
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setLoaded(true);
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  async function addRule() {
    if (!tool.trim()) return;
    try {
      await invoke("permission_add", {
        tool: tool.trim(),
        decision,
        scope: scope === "global" ? undefined : "project",
        projectId: scope === "global" ? undefined : scope,
      });
      setTool("");
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function removeRule(id: string) {
    try {
      await invoke("permission_remove", { id });
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  function scopeLabel(scope: RuleRecord["scope"]) {
    if (scope.kind === "global") return "Everywhere";
    const project = projects.find((candidate) => candidate.id === scope.project_id);
    return project ? `In “${project.name}”` : "In a deleted project";
  }

  return (
    <section className="settings-section">
      <h3>Permissions</h3>
      <p className="sidebar-hint">Rules decide tool calls before the approval mode above — “always allow websearch” means exactly that. Approvals and folder mounts always apply on top.</p>
      {error && <div className="settings-error" role="alert">{error}</div>}
      <div className="permission-rules">
        {rules.map((rule) => (
          <div className="permission-rule" key={rule.id}>
            <span className={`rule-chip ${rule.decision}`}>{rule.decision === "allow" ? "Allow" : "Block"}</span>
            <code className="rule-tool">{rule.tool}</code>
            <span className="rule-scope">{scopeLabel(rule.scope)}</span>
            <button className="icon-button folder-remove" aria-label={`Remove rule for ${rule.tool}`} title="Remove rule" onClick={() => removeRule(rule.id)}><X size={13} /></button>
          </div>
        ))}
        {loaded && !rules.length && <p className="sidebar-empty">No custom rules yet — approval cards offer “Always allow” / “Always block” to remember a choice.</p>}
      </div>
      <div className="permission-form">
        <input
          className="settings-input"
          list="rule-tool-suggestions"
          value={tool}
          placeholder="Tool name — bash, webfetch, mcp__gmail__…"
          aria-label="Tool for the new rule"
          onChange={(event) => setTool(event.target.value)}
        />
        <datalist id="rule-tool-suggestions">{RULE_TOOL_SUGGESTIONS.map((name) => <option key={name} value={name} />)}</datalist>
        <select className="settings-select" value={decision} aria-label="Decision for the new rule" onChange={(event) => setDecision(event.target.value === "deny" ? "deny" : "allow")}>
          <option value="allow">Always allow</option>
          <option value="deny">Always block</option>
        </select>
        <select className="settings-select" value={scope} aria-label="Scope for the new rule" onChange={(event) => setScope(event.target.value)}>
          <option value="global">Everywhere</option>
          {projects.map((project) => (
            <option key={project.id} value={project.id}>In “{project.name}”</option>
          ))}
        </select>
        <button type="button" className="secondary-action" disabled={!tool.trim()} onClick={addRule}><Plus size={14} /> Add rule</button>
      </div>
    </section>
  );
}

function SettingsDialog({ settings, models, daemonInfo, daemonStatus, onChange, onClose }: {
  settings: Settings;
  models: ModelRoute[];
  daemonInfo: DaemonInfo | null;
  daemonStatus: "connecting" | "connected" | "offline";
  onChange: <K extends keyof Settings>(key: K, value: Settings[K]) => void;
  onClose: () => void;
}) {
  const isKnowledge = settings.interfaceLayout === "knowledge";
  return (
    <div className="settings-overlay" onClick={onClose} role="presentation">
      <div className="settings-dialog" role="dialog" aria-modal="true" aria-label="Settings" onClick={(event) => event.stopPropagation()}>
        <header className="settings-header">
          <div><span className="feed-kicker">PREFERENCES</span><h2>Settings</h2></div>
          <button className="icon-button" aria-label="Close settings" onClick={onClose}><X size={17} /></button>
        </header>

        <section className="settings-section">
          <h3>Workspace</h3>
          <label className="settings-row">
            <span className="settings-toggle"><input type="checkbox" checked={settings.reopenFolders} onChange={(event) => onChange("reopenFolders", event.target.checked)} /><i /></span>
            <span className="settings-copy"><strong>Reopen mounted folders on launch</strong><small>Folders from your last session are remounted automatically. Nothing is sent anywhere — mounts only define the workspace boundary.</small></span>
          </label>
        </section>

        <section className="settings-section">
          <h3>Sessions</h3>
          <label className="settings-row">
            <span className="settings-toggle"><input type="checkbox" checked={settings.autoOpenArtifacts} onChange={(event) => onChange("autoOpenArtifacts", event.target.checked)} /><i /></span>
            <span className="settings-copy"><strong>Open the artifact panel on new deliverables</strong><small>Brings the preview forward whenever a task creates a file.</small></span>
          </label>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Default model</strong><small>Used for every new task. Pick from the MODELS list or here — tasks start on this model without touching the terminal.</small></span>
            <select
              className="settings-select"
              value={settings.defaultModel}
              aria-label="Default model"
              onChange={(event) => onChange("defaultModel", event.target.value)}
            >
              <option value="">Provider default</option>
              {models.filter((route) => route.available).map((route) => (
                <option key={`${route.provider}:${route.model}`} value={route.model}>{route.model} · {route.provider}</option>
              ))}
            </select>
          </div>
        </section>

        <section className="settings-section">
          <h3>General</h3>
          <p className="sidebar-hint">Configure agent execution, queued message delivery, and permissions — then choose how much interface you want around them.</p>
          <LayoutCards value={settings.interfaceLayout} onSelect={(layout) => onChange("interfaceLayout", layout)} />
        </section>

        <section className="settings-section">
          <h3>Capabilities</h3>
          <ApprovalModeRow />
          {!isKnowledge && <ToolsSection />}
          <div className="settings-row static">
            <span className="settings-copy"><strong>Folder access</strong><small>Tasks read and write only inside folders you mount — per project, with explicit read-only or read/write access. Revoking a mount takes effect immediately.</small></span>
          </div>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Destructive actions</strong><small>Shell commands, file edits, office document creation, and computer control always wait for your approval, regardless of the mode above.</small></span>
          </div>
        </section>

        <PermissionRulesSection />

        <AccountsSection />
        {!isKnowledge && <McpSection />}
        <UsageSection />

        <section className="settings-section">
          <h3>Connection</h3>
          <div className="settings-facts">
            <div><span>Daemon</span><strong>{daemonInfo ? `${daemonInfo.name} · v${String(daemonInfo.version)}` : daemonStatus === "connected" ? "connected" : daemonStatus}</strong></div>
            <div><span>Transport</span><strong>Named pipe · capability token</strong></div>
            <div><span>Daemon log</span><code>{daemonInfo?.log_path || "%TEMP%\\jcode-cowork-daemon.log"}</code></div>
            <div><span>Storage</span><strong>History and settings stay on this device</strong></div>
          </div>
        </section>
      </div>
    </div>
  );
}

/// Approval policy selector backed by the daemon (settings.get/update) so the
/// policy survives restarts and is enforced daemon-side, not just in the UI.
function ApprovalModeRow() {
  const [mode, setMode] = useState<ApprovalMode>("always_ask");
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let cancelled = false;
    invoke<{ approval_mode?: ApprovalMode }>("settings_get")
      .then((result) => {
        if (!cancelled && result?.approval_mode) setMode(result.approval_mode);
      })
      .catch((reason) => { if (!cancelled) setError(errorMessage(reason)); })
      .finally(() => { if (!cancelled) setLoaded(true); });
    return () => { cancelled = true; };
  }, []);

  function choose(next: ApprovalMode) {
    setMode(next);
    invoke("settings_update", { approvalMode: next })
      .then(() => setError(""))
      .catch((reason) => setError(errorMessage(reason)));
  }

  return (
    <div className="settings-row static approval-mode">
      <span className="settings-copy"><strong>Approval mode</strong><small>
        {mode === "always_ask"
          ? "Every gated action asks first — the strict default."
          : "Read-only tools (open files, search, web fetch) run without prompting; writes, shell commands, and computer control still ask."}
      </small></span>
      <div className="approval-mode-options">
        <button type="button" className={`approval-option ${mode === "always_ask" ? "selected" : ""}`} disabled={!loaded} onClick={() => choose("always_ask")}><ShieldCheck size={14} /> Always ask</button>
        <button type="button" className={`approval-option ${mode === "auto_allow_readonly" ? "selected" : ""}`} disabled={!loaded} onClick={() => choose("auto_allow_readonly")}><Sparkles size={14} /> Auto-allow read-only</button>
      </div>
      {error && <small className="settings-error">{error}</small>}
    </div>
  );
}

/// Live tool catalog from the daemon (tools.list): what desktop tasks can
/// use, and which entries are read-only under the auto-allow policy.
function ToolsSection() {
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [error, setError] = useState("");

  useEffect(() => {
    let cancelled = false;
    invoke<{ tools?: ToolInfo[] }>("tools_list")
      .then((result) => { if (!cancelled) setTools(Array.isArray(result?.tools) ? result.tools : []); })
      .catch((reason) => { if (!cancelled) setError(errorMessage(reason)); });
    return () => { cancelled = true; };
  }, []);

  return (
    <div className="settings-row static">
      <span className="settings-copy"><strong>Tools available to tasks</strong><small><Wrench size={11} style={{ verticalAlign: "-1px" }} /> Every desktop task runs with the full jcode tool registry. Read-only tools run without prompting in auto-allow mode; configured MCP servers add their own tools dynamically.</small></span>
      <div className="tools-chips">
        {tools.map((tool) => (
          <span className={`tool-chip ${tool.read_only ? "readonly" : ""}`} key={tool.name} title={tool.description}>
            {tool.name}{tool.read_only ? <em>read-only</em> : null}
          </span>
        ))}
        {!tools.length && !error && <small>Loading…</small>}
        {error && <small className="settings-error">{error}</small>}
      </div>
    </div>
  );
}

/// Provider sign-in surface backed by auth.status / auth.set_api_key /
/// auth.clear — the same credential truth the CLI reports, with API keys
/// editable in place and interactive logins pointed at `jcode login`.
function AccountsSection() {
  const [providers, setProviders] = useState<AuthProvider[]>([]);
  const [error, setError] = useState("");
  const [expanded, setExpanded] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  const [busyProvider, setBusyProvider] = useState("");

  function reload() {
    invoke<{ providers?: AuthProvider[] }>("auth_status")
      .then((result) => { setProviders(Array.isArray(result?.providers) ? result.providers : []); setError(""); })
      .catch((reason) => setError(errorMessage(reason)));
  }

  useEffect(reload, []);

  async function saveKey(provider: AuthProvider) {
    setBusyProvider(provider.id);
    try {
      await invoke("auth_set_api_key", { provider: provider.id, apiKey: keyDraft.trim() });
      setExpanded("");
      setKeyDraft("");
      reload();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusyProvider("");
    }
  }

  async function clearKey(provider: AuthProvider) {
    setBusyProvider(provider.id);
    try {
      await invoke("auth_clear", { provider: provider.id });
      reload();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusyProvider("");
    }
  }

  const stateLabel: Record<AuthProvider["state"], string> = {
    available: "ready",
    expired: "expired",
    not_configured: "not set up",
  };

  return (
    <section className="settings-section">
      <h3>Accounts</h3>
      <p className="sidebar-hint">Sign in to providers here or with the jcode CLI — both read the same credentials. API keys can be set and cleared in place; OAuth/device logins run in a terminal with <code>jcode login &lt;provider&gt;</code>.</p>
      <div className="accounts-list">
        {providers.map((provider) => (
          <div className="account-row" key={provider.id}>
            <div className="account-line">
              <span className={`account-state ${provider.state}`} />
              <span className="account-name">{provider.display_name}</span>
              <span className="account-detail" title={provider.health_summary}>{provider.method_detail}</span>
              <span className={`account-badge ${provider.state}`}>{stateLabel[provider.state]}</span>
              {provider.api_key_settable && (
                <span className="account-actions">
                  {expanded === provider.id ? (
                    <button className="icon-button small" aria-label="Cancel key entry" onClick={() => { setExpanded(""); setKeyDraft(""); }}><X size={13} /></button>
                  ) : (
                    <button className="icon-button small" aria-label={`Set API key for ${provider.display_name}`} title="Set API key" onClick={() => { setExpanded(provider.id); setKeyDraft(""); }}><KeyRound size={13} /></button>
                  )}
                  <button className="icon-button small" aria-label={`Clear credentials for ${provider.display_name}`} title="Clear stored key" disabled={busyProvider === provider.id} onClick={() => clearKey(provider)}><Trash2 size={13} /></button>
                </span>
              )}
            </div>
            {expanded === provider.id && (
              <div className="account-key-row">
                <input
                  type="password"
                  className="settings-input"
                  placeholder={`${provider.display_name} API key`}
                  aria-label={`${provider.display_name} API key`}
                  value={keyDraft}
                  autoFocus
                  onChange={(event) => setKeyDraft(event.target.value)}
                  onKeyDown={(event) => { if (event.key === "Enter" && keyDraft.trim()) saveKey(provider); }}
                />
                <button className="primary-action small" disabled={!keyDraft.trim() || busyProvider === provider.id} onClick={() => saveKey(provider)}>{busyProvider === provider.id ? <LoaderCircle className="spin" size={13} /> : <KeyRound size={13} />} Save key</button>
              </div>
            )}
          </div>
        ))}
        {!providers.length && !error && <p className="sidebar-empty">Reading credentials…</p>}
        {error && <p className="sidebar-empty settings-error">{error}</p>}
      </div>
    </section>
  );
}

/// MCP server manager backed by mcp.list / mcp.add / mcp.remove on the global
/// ~/.jcode/mcp.json. Tasks pick these up as mcp__server__tool entries.
function McpSection() {
  const [servers, setServers] = useState<McpServerInfo[]>([]);
  const [configPath, setConfigPath] = useState("");
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [url, setUrl] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  function reload() {
    invoke<{ servers?: McpServerInfo[]; config_path?: string }>("mcp_list")
      .then((result) => {
        setServers(Array.isArray(result?.servers) ? result.servers : []);
        if (result?.config_path) setConfigPath(result.config_path);
      })
      .catch((reason) => setError(errorMessage(reason)));
  }

  useEffect(reload, []);

  async function addServer(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const trimmedArgs = args.trim() ? args.trim().split(/\s+/) : undefined;
      await invoke("mcp_add", {
        name: name.trim(),
        command: command.trim() || undefined,
        args: trimmedArgs,
        url: url.trim() || undefined,
      });
      setName(""); setCommand(""); setArgs(""); setUrl("");
      reload();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function removeServer(server: McpServerInfo) {
    if (!window.confirm(`Remove MCP server “${server.name}” from ${configPath}?`)) return;
    try {
      await invoke("mcp_remove", { name: server.name });
      reload();
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  return (
    <section className="settings-section">
      <h3>MCP servers</h3>
      <p className="sidebar-hint">Model Context Protocol servers give tasks extra tools. Stored in <code>{configPath || "~/.jcode/mcp.json"}</code>; every new task registers them automatically.</p>
      <div className="mcp-list">
        {servers.map((server) => (
          <div className="mcp-row" key={server.name}>
            <span className={`mcp-state ${server.enabled ? "on" : "off"}`} />
            <span className="mcp-name">{server.name}</span>
            <span className="mcp-target" title={server.command || server.url}>{server.command ? `${server.command} ${(server.args || []).join(" ")}` : server.url}</span>
            <span className="mcp-transport">{server.transport}</span>
            <button className="icon-button folder-remove" aria-label={`Remove ${server.name}`} title="Remove server" onClick={() => removeServer(server)}><X size={13} /></button>
          </div>
        ))}
        {!servers.length && <p className="sidebar-empty">No MCP servers configured yet.</p>}
      </div>
      <form className="mcp-add" onSubmit={addServer}>
        <div className="mcp-add-grid">
          <input className="settings-input" value={name} aria-label="Server name" placeholder="name (letters, digits, - _ .)" onChange={(event) => setName(event.target.value)} />
          <input className="settings-input" value={command} aria-label="Command for a stdio server" placeholder="command (stdio), e.g. npx -y some-mcp" onChange={(event) => { setCommand(event.target.value); setUrl(""); }} />
          <input className="settings-input" value={url} aria-label="URL for a remote server" placeholder="or https://… (remote http/sse)" onChange={(event) => { setUrl(event.target.value); setCommand(""); }} />
          <input className="settings-input" value={args} aria-label="Command arguments" placeholder="args (space-separated)" onChange={(event) => setArgs(event.target.value)} />
        </div>
        {error && <small className="settings-error">{error}</small>}
        <button type="submit" className="secondary-action" disabled={busy || !name.trim() || (!command.trim() && !url.trim())}>{busy ? <LoaderCircle className="spin" size={13} /> : <Plus size={13} />} Add server</button>
      </form>
    </section>
  );
}

/// Provider usage limits (usage.report): live windows per provider account
/// plus the Anthropic five-hour / seven-day utilizations.
function UsageSection() {
  const [report, setReport] = useState<UsageReport | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    let cancelled = false;
    invoke<UsageReport>("usage_report")
      .then((result) => { if (!cancelled) setReport(result); })
      .catch((reason) => { if (!cancelled) setError(errorMessage(reason)); });
    return () => { cancelled = true; };
  }, []);

  if (error) {
    return (
      <section className="settings-section">
        <h3>Usage</h3>
        <p className="sidebar-empty settings-error">{error}</p>
      </section>
    );
  }
  if (!report) {
    return (
      <section className="settings-section">
        <h3>Usage</h3>
        <p className="sidebar-empty">Reading provider usage…</p>
      </section>
    );
  }

  const claude = report.claude_windows;
  const providersWithLimits = report.providers.filter((provider) => provider.limits.length);

  return (
    <section className="settings-section">
      <h3>Usage</h3>
      {providersWithLimits.length ? (
        <div className="usage-list">
          {providersWithLimits.map((provider) => (
            <div className="usage-provider" key={provider.provider_name}>
              <div className="usage-provider-name">{provider.provider_name}</div>
              {provider.limits.map((limit) => (
                <div className="usage-limit" key={`${provider.provider_name}:${limit.name}`}>
                  <span className="usage-label">{limit.name}{limit.resets_at ? ` · resets ${limit.resets_at}` : ""}</span>
                  <span className="usage-bar"><i style={{ width: `${Math.min(100, Math.max(2, limit.usage_percent))}%`, background: limit.usage_percent >= 99 ? "var(--clay)" : "var(--moss)" }} /></span>
                  <span className="usage-value">{Math.round(limit.usage_percent)}%</span>
                </div>
              ))}
              {provider.error && <small className="settings-error">{provider.error}</small>}
            </div>
          ))}
        </div>
      ) : <p className="sidebar-empty">No provider reports usage windows yet — they appear once a subscription provider is signed in.</p>}
      {claude && (claude.five_hour > 0 || claude.seven_day > 0) && (
        <div className="usage-provider">
          <div className="usage-provider-name">Anthropic subscription windows</div>
          <div className="usage-limit">
            <span className="usage-label">Five-hour{claude.five_hour_resets_at ? ` · resets ${claude.five_hour_resets_at}` : ""}</span>
            <span className="usage-bar"><i style={{ width: `${Math.min(100, Math.max(2, claude.five_hour * 100))}%`, background: claude.five_hour >= 0.99 ? "var(--clay)" : "var(--moss)" }} /></span>
            <span className="usage-value">{Math.round(claude.five_hour * 100)}%</span>
          </div>
          <div className="usage-limit">
            <span className="usage-label">Seven-day{claude.seven_day_resets_at ? ` · resets ${claude.seven_day_resets_at}` : ""}</span>
            <span className="usage-bar"><i style={{ width: `${Math.min(100, Math.max(2, claude.seven_day * 100))}%`, background: claude.seven_day >= 0.99 ? "var(--clay)" : "var(--moss)" }} /></span>
            <span className="usage-value">{Math.round(claude.seven_day * 100)}%</span>
          </div>
        </div>
      )}
      <p className="sidebar-hint"><BarChart3 size={11} style={{ verticalAlign: "-1px" }} /> Same data as <code>jcode usage</code>, cached by the daemon.</p>
    </section>
  );
}

/// Schedule a project-scoped task for later; the daemon fires it via its
/// 30-second scheduler loop with the project's mounts and instructions.
function ScheduleDialog({ projectId, projectName, defaultModel, onClose, onSchedule }: {
  projectId: string;
  projectName: string;
  defaultModel: string;
  onClose: () => void;
  onSchedule: (projectId: string, goal: string, runAtUnixSecs: number, model?: string) => Promise<void>;
}) {
  const [goal, setGoal] = useState("");
  const [when, setWhen] = useState("");
  const [model, setModel] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!goal.trim() || !when) return;
    const runAt = Math.floor(new Date(when).getTime() / 1000);
    if (!Number.isFinite(runAt)) {
      setError("Pick a valid date and time.");
      return;
    }
    if (runAt * 1000 < Date.now()) {
      setError("Pick a time in the future.");
      return;
    }
    setBusy(true);
    setError("");
    try {
      await onSchedule(projectId, goal.trim(), runAt, model || undefined);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="settings-overlay" onClick={onClose} role="presentation">
      <form className="settings-dialog" role="dialog" aria-modal="true" aria-label="Schedule a task" onSubmit={submit} onClick={(event) => event.stopPropagation()}>
        <header className="settings-header">
          <div><span className="feed-kicker">SCHEDULED TASK</span><h2>Run a goal later in “{projectName}”</h2></div>
          <button type="button" className="icon-button" aria-label="Close schedule dialog" onClick={onClose}><X size={17} /></button>
        </header>
        <section className="settings-section">
          <h3>Goal</h3>
          <textarea
            className="settings-textarea"
            value={goal}
            rows={3}
            autoFocus
            aria-label="Scheduled goal"
            placeholder="What should run when the time comes?"
            onChange={(event) => setGoal(event.target.value)}
          />
        </section>
        <section className="settings-section">
          <h3>When</h3>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Run at</strong><small>The desktop daemon fires the task within 30 seconds of this time, using the project's mounted folders and instructions.</small></span>
            <input className="settings-input" type="datetime-local" value={when} aria-label="Run at" onChange={(event) => setWhen(event.target.value)} />
          </div>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Model</strong><small>Optional override; the project's default model applies otherwise.</small></span>
            <select className="settings-select" value={model} aria-label="Scheduled task model" onChange={(event) => setModel(event.target.value)}>
              <option value="">{defaultModel ? `Project default · ${defaultModel}` : "Project default"}</option>
            </select>
          </div>
        </section>
        {error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{error}</span></div>}
        <footer className="settings-footer">
          <button type="button" className="secondary-action" onClick={onClose}>Cancel</button>
          <button type="submit" className="primary-action" disabled={busy || !goal.trim() || !when}>{busy ? <LoaderCircle className="spin" size={15} /> : <CalendarClock size={15} />} Schedule</button>
        </footer>
      </form>
    </div>
  );
}

/// Full conversation transcript for a finished or running task, read from the
/// daemon's persisted session store (task.transcript).
function TranscriptDialog({ view, onClose }: { view: TranscriptView; onClose: () => void }) {
  const [entries, setEntries] = useState<TranscriptEntry[] | null>(null);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState("");

  useEffect(() => {
    let cancelled = false;
    invoke<{ messages?: TranscriptEntry[]; total_messages?: number }>("task_transcript", { taskId: view.taskId, limit: 400 })
      .then((result) => {
        if (cancelled) return;
        setEntries(Array.isArray(result?.messages) ? result.messages : []);
        setTotal(typeof result?.total_messages === "number" ? result.total_messages : 0);
      })
      .catch((reason) => { if (!cancelled) setError(errorMessage(reason)); });
    return () => { cancelled = true; };
  }, [view.taskId]);

  return (
    <div className="settings-overlay" onClick={onClose} role="presentation">
      <div className="settings-dialog transcript-dialog" role="dialog" aria-modal="true" aria-label="Task transcript" onClick={(event) => event.stopPropagation()}>
        <header className="settings-header">
          <div><span className="feed-kicker">TRANSCRIPT</span><h2>{view.goal}</h2></div>
          <button className="icon-button" aria-label="Close transcript" onClick={onClose}><X size={17} /></button>
        </header>
        <div className="transcript-body">
          {error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{error}</span></div>}
          {!error && !entries && <div className="preview-message"><LoaderCircle className="spin" size={19} /><span>Loading transcript…</span></div>}
          {entries && !entries.length && <p className="sidebar-empty">The session has no visible conversation yet — try again once the task has produced output.</p>}
          {entries && entries.length > 0 && (
            <>
              {total > entries.length && <p className="sidebar-hint">Showing the last {entries.length} of {total} conversation messages.</p>}
              <div className="transcript-list">
                {entries.map((entry, index) => (
                  <article className={`transcript-entry ${entry.role}`} key={index}>
                    <span className="transcript-role">{entry.role === "user" ? "You" : "Agent"}</span>
                    <div className="transcript-content">
                      {entry.text && <p>{entry.text}</p>}
                      {entry.events.map((event, eventIndex) => (
                        <div className={`transcript-event ${event.is_error ? "error" : ""}`} key={eventIndex}>
                          {event.type === "tool_use"
                            ? <><ChevronRight size={12} /> used <code>{event.tool}</code></>
                            : <><FileText size={12} /> {event.type === "tool_result" ? `result${event.is_error ? " (error)" : ""}` : event.type}: {event.text}</>}
                        </div>
                      ))}
                    </div>
                  </article>
                ))}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function iconForArtifact(path: string) {
  const extension = path.split(".").pop()?.toLowerCase();
  return extension === "xlsx" || extension === "xls" ? <FileSpreadsheet size={15} /> : <FileText size={15} />;
}

function FeedCard({ entry, handled, plain, projectName, onAnswer }: { entry: FeedEntry; handled: Record<string, boolean>; plain?: boolean; projectName?: string; onAnswer: (id: string | number, approved: boolean, remember?: "global" | "project") => void }) {
  const [expanded, setExpanded] = useState(true);
  const label = entry.event === "AgentSpawned" ? "Agent joined" : entry.event === "TaskDelta" ? (plain ? "Narrating" : "Live narrative") : entry.event === "ToolInvoked" ? (plain ? "Working" : "Tool activity") : entry.event === "ArtifactCreated" ? "Artifact created" : entry.event === "ApprovalRequired" ? "Permission requested" : "Task finished";
  const approvalId = entry.data.id;
  const approvalKey = identifier(entry.data, "id");
  const approvalHandled = approvalKey in handled;
  const agentName = text(entry.data, "agent", "agent_name", "name") || "Cworkspace agent";
  const rawTool = text(entry.data, "tool", "tool_name");
  const toolName = rawTool || "Tool";
  // Knowledge Work shows what the agent is doing, not which tool it uses.
  const displayTool = plain && rawTool ? toolPhrase(rawTool) : toolName;
  const description = text(entry.data, "description", "summary", "message", "input", "arguments");
  const artifactPath = text(entry.data, "path");
  const approvalReady = typeof approvalId === "string" || typeof approvalId === "number";

  return (
    <article className={`feed-entry ${entry.event.toLowerCase()}`}>
      <div className="timeline-rail"><span className={`event-symbol ${entry.event.toLowerCase()}`}>{entry.event === "AgentSpawned" ? <Sparkles size={14} /> : entry.event === "TaskDelta" ? <Pencil size={14} /> : entry.event === "ToolInvoked" ? <ChevronRight size={14} /> : entry.event === "ArtifactCreated" ? <FileText size={14} /> : entry.event === "ApprovalRequired" ? <ShieldCheck size={14} /> : <Check size={14} />}</span><span className="rail-line" /></div>
      <div className="event-body">
        <button className="event-title-row" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)}><div><span className="event-label">{label}</span><strong>{entry.event === "AgentSpawned" ? agentName : entry.event === "TaskDelta" ? agentName : entry.event === "ToolInvoked" ? displayTool : entry.event === "ArtifactCreated" ? folderName(artifactPath || "New file") : entry.event === "ApprovalRequired" ? (rawTool ? displayTool : "Action needs your approval") : "This task is complete"}</strong></div><span className="event-time"><Clock3 size={12} /> {new Date(entry.createdAt).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })}{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</span></button>
        {expanded && <div className="event-detail">
          {entry.event === "TaskDelta" ? <p className="narrative-text">{String(entry.data.text ?? "")}</p>
          : entry.event === "ApprovalRequired" ? <div className={`approval-card ${approvalHandled ? "answered" : ""}`}><div className="approval-copy"><span className="approval-eyebrow"><ShieldCheck size={13} /> {plain ? "PERMISSION NEEDED" : "YOUR DECISION"}</span><p>{description || (plain ? `Cworkspace wants to ${rawTool ? toolPhrase(rawTool).toLowerCase() : "continue with this action"}. Allow it?` : "An agent is requesting permission to continue with this action.")}</p>{rawTool && <code>{displayTool}</code>}</div>{approvalHandled ? <div className="approval-result"><Check size={14} /> {handled[approvalKey] ? "Allowed" : "Rejected"}</div> : <div className="approval-actions-wrap"><div className="approval-actions"><button className="reject-button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, false)}><X size={14} /> {plain ? "No thanks" : "Reject"}</button><button className="allow-button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, true)}><Check size={14} /> Allow</button></div><div className="approval-remember"><button type="button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, true, projectName ? "project" : "global")}><Check size={12} /> Always allow</button><button type="button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, false, projectName ? "project" : "global")}><X size={12} /> Always block</button><small>for {projectName ? `“${projectName}”` : "everywhere"}</small></div></div>}</div>
          : entry.event === "ArtifactCreated" ? <p className="event-description">{artifactPath || description || "A new deliverable is ready to preview."}</p>
          : <p className="event-description">{description || (entry.event === "AgentSpawned" ? "Preparing a focused workstream." : entry.event === "ToolInvoked" ? "Working through the requested step." : "The requested work has finished.")}</p>}
        </div>}
      </div>
    </article>
  );
}

function ArtifactPreview({ artifact }: { artifact: Artifact | null }) {
  const [preview, setPreview] = useState<PreviewState | null>(null);

  useEffect(() => {
    if (!artifact) {
      setPreview(null);
      return;
    }
    let cancelled = false;
    setPreview({ status: "loading" });
    invoke<ArtifactPayload>("read_artifact", { path: artifact.path })
      .then(async (payload) => {
        if (cancelled) return;
        const buffer = decodeBase64(payload.content_base64);
        if (payload.mime_type === "application/pdf") {
          setPreview({ status: "pdf", payload, url: `data:application/pdf;base64,${payload.content_base64}` });
        } else if (payload.mime_type === "text/markdown") {
          setPreview({ status: "ready", payload, markdown: new TextDecoder().decode(buffer) });
        } else if (payload.mime_type.includes("spreadsheetml") || artifact.path.toLowerCase().endsWith(".xls")) {
          const workbook = XLSX.read(buffer, { type: "array" });
          const sheet = workbook.Sheets[workbook.SheetNames[0]];
          const rows = sheet ? XLSX.utils.sheet_to_json<unknown[]>(sheet, { header: 1, defval: "", blankrows: false }) : [];
          const columns = rows.reduce((largest, row) => Math.max(largest, row.length), 0);
          setPreview({ status: "ready", payload, rows: rows.slice(0, 100), columns });
        } else if (payload.mime_type.includes("wordprocessingml")) {
          const result = await mammoth.convertToHtml({ arrayBuffer: buffer });
          if (!cancelled) setPreview({ status: "ready", payload, docxHtml: DOMPurify.sanitize(result.value) });
        } else {
          // No inline renderer for this type (pptx, text/code, anything the
          // task produced) — offer a real download instead of a dead end.
          setPreview({ status: "ready", payload });
        }
      })
      .catch((reason: unknown) => {
        if (!cancelled) setPreview({ status: "error", message: errorMessage(reason) });
      });
    return () => { cancelled = true; };
  }, [artifact]);

  if (!artifact) return <div className="preview-empty"><div className="preview-illustration"><FileText size={28} /><span /><i /></div><strong>A clear view of what’s done.</strong><p>Created files will appear here, ready to read without leaving your workspace.</p><div className="supported-formats"><span>MD</span><span>PDF</span><span>XLSX</span><span>DOCX</span><span>PPTX</span></div></div>;
  if (!preview || preview.status === "loading") return <div className="preview-message"><LoaderCircle className="spin" size={19} /><span>Opening {folderName(artifact.path)}…</span></div>;
  if (preview.status === "error") return <div className="preview-message preview-error"><AlertCircle size={18} /><span>{preview.message}</span></div>;
  if (preview.status === "pdf") return <div className="pdf-preview"><div className="preview-file-title"><FileText size={15} /><span>{preview.payload.name}</span></div><iframe title={`Preview of ${preview.payload.name}`} src={preview.url} /></div>;
  if (preview.rows) return <div className="spreadsheet-preview"><div className="preview-file-title"><FileSpreadsheet size={15} /><span>{preview.payload.name}</span></div>{preview.rows.length ? <div className="sheet-scroll"><table><tbody>{preview.rows.map((row, rowIndex) => <tr key={rowIndex}>{Array.from({ length: preview.columns || 1 }, (_, columnIndex) => { const Cell = rowIndex === 0 ? "th" : "td"; return <Cell key={columnIndex}>{String(row[columnIndex] ?? "")}</Cell>; })}</tr>)}</tbody></table></div> : <div className="preview-message">This workbook has no visible rows.</div>}</div>;
  if (preview.docxHtml) return <div className="document-preview"><div className="preview-file-title"><FileText size={15} /><span>{preview.payload.name}</span></div><article className="docx-content" dangerouslySetInnerHTML={{ __html: preview.docxHtml }} /></div>;
  if (!preview.markdown && !preview.rows) return <div className="download-preview"><div className="preview-file-title"><FileText size={15} /><span>{preview.payload.name}</span></div><p className="sidebar-hint">No inline preview for this file type — download it to open in the app it was made for.</p><a className="primary-action" download={preview.payload.name} href={`data:${preview.payload.mime_type || "application/octet-stream"};base64,${preview.payload.content_base64}`}><FileDown size={15} /> Download {preview.payload.name}</a></div>;
  return <div className="markdown-preview"><div className="preview-file-title"><FileText size={15} /><span>{preview.payload.name}</span></div><article className="markdown-content"><ReactMarkdown>{preview.markdown || ""}</ReactMarkdown></article></div>;
}

export default App;
