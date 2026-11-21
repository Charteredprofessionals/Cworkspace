import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import {
  AlertCircle,
  ArrowDown,
  ArrowUpRight,
  Check,
  ChevronDown,
  ChevronRight,
  CircleHelp,
  Clock3,
  FileSpreadsheet,
  FileText,
  Folder,
  FolderPlus,
  History,
  LoaderCircle,
  PanelRightClose,
  Play,
  Plus,
  Settings,
  ShieldCheck,
  Sparkles,
  StopCircle,
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
  event: "AgentSpawned" | "ToolInvoked" | "ArtifactCreated" | "ApprovalRequired" | "TaskCompleted";
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

type SessionTab = {
  id: string;
  title: string;
  goal: string;
  status: TabStatus;
  taskId: string;
  feed: FeedEntry[];
  artifacts: Artifact[];
  selectedArtifact: Artifact | null;
  handledApprovals: Record<string, boolean>;
  error: string;
  notice: string;
};

type Settings = {
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

/// Must match `cowork_rpc::PROTOCOL_VERSION`. The daemon bumps it on breaking
/// RPC changes; the GUI warns instead of failing mysteriously on a mismatch.
const PROTOCOL_VERSION = 2;

const HISTORY_KEY = "jcode-cowork-history-v1";
const SETTINGS_KEY = "cworkspace-settings-v1";
const FOLDERS_KEY = "cworkspace-folders-v1";

const DEFAULT_SETTINGS: Settings = {
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

let tabCounter = 0;
function newTab(): SessionTab {
  tabCounter += 1;
  return {
    id: `tab-${tabCounter}-${Date.now().toString(16)}`,
    title: "New session",
    goal: "",
    status: "idle",
    taskId: "",
    feed: [],
    artifacts: [],
    selectedArtifact: null,
    handledApprovals: {},
    error: "",
    notice: "",
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
  const [protocolWarning, setProtocolWarning] = useState("");
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
    invoke("connect_daemon")
      .then(async () => {
        if (cancelled) return;
        setDaemonStatus("connected");
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
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<CoworkEvent>("cowork-event", ({ payload }) => {
      const entry: FeedEntry = {
        key: `${Date.now()}-${Math.random().toString(16).slice(2)}`,
        event: payload.event,
        data: payload.data || {},
        createdAt: Date.now(),
      };
      const taskId = identifier(entry.data, "task_id");
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

  async function startTask(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const tab = activeTab;
    if (!tab) return;
    const goal = tab.goal.trim();
    if (!goal) return;
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
      title: tabTitle(goal),
    });
    try {
      const taskId = await invoke<string>("start_cowork_task", {
        goal,
        projectId: activeProject ? activeProject.id : undefined,
        allowedRoots: activeProject ? [] : folders,
        model: settings.defaultModel || undefined,
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
        model: settings.defaultModel || undefined,
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

  async function answerApproval(tabId: string, id: string | number, approved: boolean) {
    try {
      await invoke("respond_approval", { id, approved });
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
            <div className="section-heading"><span>PROJECT FOLDERS</span><FolderPlus size={14} /></div>
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

        <section className="sidebar-section models-section">
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
        </section>

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
            <div className="goal-form-top"><Sparkles size={16} /><label htmlFor="goal-input">What would you like to get done?</label>{(activeProject ? activeProject.mounts.length : folders.length) > 0 && <span className="root-count">{activeProject ? `${activeProject.name} · ${activeProject.mounts.length} ${activeProject.mounts.length === 1 ? "folder" : "folders"}` : `${folders.length} ${folders.length === 1 ? "folder" : "folders"} in scope`}</span>}</div>
            <textarea id="goal-input" ref={goalInputRef} value={activeTab?.goal || ""} onChange={(event) => activeTab && patchTab(activeTab.id, { goal: event.target.value })} placeholder="Draft a project brief, audit these figures, prepare a polished report…" rows={3} disabled={activeTab?.status === "running"} />
            <div className="goal-form-bottom">
              <span><CircleHelp size={14} /> {settings.defaultModel ? `Model: ${settings.defaultModel}` : activeProject ? `Runs inside “${activeProject.name}” with its instructions and folders.` : "Your mounted folders define the workspace boundary."}</span>
              <button className="primary-action" type="submit" disabled={busy || activeTab?.status === "running" || !activeTab?.goal.trim()}>{busy ? <LoaderCircle className="spin" size={16} /> : <Play size={15} fill="currentColor" />} Start task <ArrowUpRight size={15} /></button>
            </div>
          </form>

          {protocolWarning && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{protocolWarning}</span><button className="icon-button" aria-label="Dismiss warning" onClick={() => setProtocolWarning("")}><X size={15} /></button></div>}
          {activeTab?.notice && <div className="info-banner" role="status"><Sparkles size={15} /><span>{activeTab.notice}</span><button className="icon-button" aria-label="Dismiss notice" onClick={() => patchTab(activeTab.id, { notice: "" })}><X size={14} /></button></div>}
          {activeTab?.error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{activeTab.error}</span><button className="icon-button" aria-label="Dismiss error" onClick={() => patchTab(activeTab.id, { error: "" })}><X size={15} /></button></div>}

          <div className="feed-heading"><div><span className="feed-kicker">LIVE SESSION</span><h2>{activeTab?.status === "running" ? "Work in motion" : activeTab?.status === "completed" ? "Session complete" : activeTab?.status === "stopped" || activeTab?.status === "failed" ? "Session stopped" : "Session"}</h2></div><div className="feed-actions">{activeTab?.status === "running" && <button className="stop-action" onClick={stopTask}><StopCircle size={15} /> Stop task</button>}<span className={`task-badge ${activeTab?.status || "idle"}`}>{activeTab?.status === "running" ? <><i className="pulse-dot" /> In progress</> : activeTab?.status === "completed" ? "Completed" : activeTab?.status === "stopped" ? "Stopped" : activeTab?.status === "failed" ? "Failed" : "Waiting"}</span></div></div>

          <div className="feed-list">
            {activeTab?.feed.map((entry) => <FeedCard key={entry.key} entry={entry} handled={activeTab.handledApprovals} onAnswer={(id, approved) => answerApproval(activeTab.id, id, approved)} />)}
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

      {pendingApprovals.length > 0 && <div className="approval-counter"><span className="pulse-dot" /> {pendingApprovals.length} decision{pendingApprovals.length === 1 ? "" : "s"} needed</div>}

      {settingsOpen && <SettingsDialog
        settings={settings}
        models={models}
        daemonInfo={daemonInfo}
        daemonStatus={daemonStatus}
        onChange={updateSetting}
        onClose={() => setSettingsOpen(false)}
      />}

      {projectDialogOpen && <ProjectDialog
        onClose={() => setProjectDialogOpen(false)}
        onCreate={createProject}
      />}
    </main>
  </ErrorBoundary>);
}

function ProjectDialog({ onClose, onCreate }: {
  onClose: () => void;
  onCreate: (name: string, purpose: string, instructions: string) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [purpose, setPurpose] = useState("");
  const [instructions, setInstructions] = useState("");
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
      await onCreate(name.trim(), purpose.trim(), instructions);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="settings-overlay" onClick={onClose} role="presentation">
      <form className="settings-dialog" role="dialog" aria-modal="true" aria-label="New project" onSubmit={submit} onClick={(event) => event.stopPropagation()}>
        <header className="settings-header">
          <div><span className="feed-kicker">NEW PROJECT</span><h2>Create a project</h2></div>
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
          <p className="sidebar-hint">Injected into every task the project starts. You can change it later in project settings.</p>
        </section>
        {error && <div className="error-banner" role="alert"><AlertCircle size={16} /><span>{error}</span></div>}
        <footer className="settings-footer">
          <button type="button" className="secondary-action" onClick={onClose}>Cancel</button>
          <button type="submit" className="primary-action" disabled={busy || !name.trim()}>{busy ? <LoaderCircle className="spin" size={15} /> : <Plus size={15} />} Create project</button>
        </footer>
      </form>
    </div>
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
          <h3>Capabilities</h3>
          <ApprovalModeRow />
          <div className="settings-row static">
            <span className="settings-copy"><strong>Folder access</strong><small>Tasks read and write only inside folders you mount — per project, with explicit read-only or read/write access. Revoking a mount takes effect immediately.</small></span>
          </div>
          <div className="settings-row static">
            <span className="settings-copy"><strong>Destructive actions</strong><small>Shell commands, file edits, office document creation, and computer control always wait for your approval, regardless of the mode above.</small></span>
          </div>
        </section>

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

function iconForArtifact(path: string) {
  const extension = path.split(".").pop()?.toLowerCase();
  return extension === "xlsx" || extension === "xls" ? <FileSpreadsheet size={15} /> : <FileText size={15} />;
}

function FeedCard({ entry, handled, onAnswer }: { entry: FeedEntry; handled: Record<string, boolean>; onAnswer: (id: string | number, approved: boolean) => void }) {
  const [expanded, setExpanded] = useState(true);
  const label = entry.event === "AgentSpawned" ? "Agent joined" : entry.event === "ToolInvoked" ? "Tool activity" : entry.event === "ArtifactCreated" ? "Artifact created" : entry.event === "ApprovalRequired" ? "Permission requested" : "Task finished";
  const approvalId = entry.data.id;
  const approvalKey = identifier(entry.data, "id");
  const approvalHandled = approvalKey in handled;
  const agentName = text(entry.data, "agent", "agent_name", "name") || "Cworkspace agent";
  const toolName = text(entry.data, "tool", "tool_name", "name") || "Tool";
  const description = text(entry.data, "description", "summary", "message", "input", "arguments");
  const artifactPath = text(entry.data, "path");
  const approvalReady = typeof approvalId === "string" || typeof approvalId === "number";

  return (
    <article className={`feed-entry ${entry.event.toLowerCase()}`}>
      <div className="timeline-rail"><span className={`event-symbol ${entry.event.toLowerCase()}`}>{entry.event === "AgentSpawned" ? <Sparkles size={14} /> : entry.event === "ToolInvoked" ? <ChevronRight size={14} /> : entry.event === "ArtifactCreated" ? <FileText size={14} /> : entry.event === "ApprovalRequired" ? <ShieldCheck size={14} /> : <Check size={14} />}</span><span className="rail-line" /></div>
      <div className="event-body">
        <button className="event-title-row" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)}><div><span className="event-label">{label}</span><strong>{entry.event === "AgentSpawned" ? agentName : entry.event === "ToolInvoked" ? toolName : entry.event === "ArtifactCreated" ? folderName(artifactPath || "New file") : entry.event === "ApprovalRequired" ? text(entry.data, "tool", "tool_name") || "Action needs your approval" : "This task is complete"}</strong></div><span className="event-time"><Clock3 size={12} /> {new Date(entry.createdAt).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })}{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</span></button>
        {expanded && <div className="event-detail">
          {entry.event === "ApprovalRequired" ? <div className={`approval-card ${approvalHandled ? "answered" : ""}`}><div className="approval-copy"><span className="approval-eyebrow"><ShieldCheck size={13} /> YOUR DECISION</span><p>{description || "An agent is requesting permission to continue with this action."}</p>{text(entry.data, "tool", "tool_name") && <code>{text(entry.data, "tool", "tool_name")}</code>}</div>{approvalHandled ? <div className="approval-result"><Check size={14} /> {handled[approvalKey] ? "Allowed" : "Rejected"}</div> : <div className="approval-actions"><button className="reject-button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, false)}><X size={14} /> Reject</button><button className="allow-button" disabled={!approvalReady} onClick={() => approvalReady && onAnswer(approvalId, true)}><Check size={14} /> Allow</button></div>}</div>
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
          setPreview({ status: "error", message: "This file type does not have a preview." });
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
  return <div className="markdown-preview"><div className="preview-file-title"><FileText size={15} /><span>{preview.payload.name}</span></div><article className="markdown-content"><ReactMarkdown>{preview.markdown || ""}</ReactMarkdown></article></div>;
}

export default App;
