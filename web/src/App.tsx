import { useEffect, useState } from "react";
import { apiGet, getToken, setToken } from "./api";
import Dashboard from "./pages/Dashboard";
import Groups from "./pages/Groups";
import Persona from "./pages/Persona";
import Settings from "./pages/Settings";
import Stickers from "./pages/Stickers";

const TABS = [
  { id: "dashboard", label: "总览" },
  { id: "groups", label: "群管理" },
  { id: "persona", label: "人设卡" },
  { id: "stickers", label: "表情" },
  { id: "settings", label: "设置" },
] as const;

type TabId = (typeof TABS)[number]["id"];

export default function App() {
  const [authed, setAuthed] = useState(getToken() !== null);
  const [tab, setTab] = useState<TabId>("dashboard");

  useEffect(() => {
    const onExpired = () => setAuthed(false);
    window.addEventListener("auth-expired", onExpired);
    return () => window.removeEventListener("auth-expired", onExpired);
  }, []);

  if (!authed) {
    return <Login onDone={() => setAuthed(true)} />;
  }

  return (
    <div className="shell">
      <header className="topbar">
        <h1>gemini-qq-bot 管理台</h1>
        <button
          type="button"
          onClick={() => {
            setToken(null);
            setAuthed(false);
          }}
        >
          退出
        </button>
      </header>
      <nav aria-label="主导航">
        <ul role="tablist" className="tabs">
          {TABS.map((t) => (
            <li key={t.id} role="presentation">
              <button
                type="button"
                role="tab"
                id={`tab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls={`panel-${t.id}`}
                className={tab === t.id ? "tab active" : "tab"}
                onClick={() => setTab(t.id)}
              >
                {t.label}
              </button>
            </li>
          ))}
        </ul>
      </nav>
      <main
        role="tabpanel"
        id={`panel-${tab}`}
        aria-labelledby={`tab-${tab}`}
        className="panel"
      >
        {tab === "dashboard" && <Dashboard />}
        {tab === "groups" && <Groups />}
        {tab === "persona" && <Persona />}
        {tab === "stickers" && <Stickers />}
        {tab === "settings" && <Settings />}
      </main>
    </div>
  );
}

function Login({ onDone }: { onDone: () => void }) {
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setToken(value.trim());
    // Probe a cheap endpoint: a wrong token bounces straight back here
    // through the 401 path in the API client.
    try {
      await apiGet("/settings");
      onDone();
    } catch {
      setToken(null);
      setError("令牌不正确，或服务不可达。");
    }
  }

  return (
    <main className="login">
      <h1>gemini-qq-bot 管理台</h1>
      <form onSubmit={(e) => void submit(e)} aria-labelledby="login-title">
        <h2 id="login-title">输入管理令牌</h2>
        <label className="field">
          <span>管理令牌（部署时配置的 ADMIN_TOKEN）</span>
          <input
            type="password"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            autoComplete="current-password"
            required
          />
        </label>
        {error && (
          <p role="alert" className="error-box">
            {error}
          </p>
        )}
        <button type="submit">进入管理台</button>
        <p className="muted">令牌只保存在本标签页的会话存储里，关掉标签页即清除。</p>
      </form>
    </main>
  );
}
