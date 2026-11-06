import { useEffect, useState } from "react";
import { apiGet, apiSend, type ModelOption, type Settings as SettingsData } from "../api";
import { ErrorBox, Loading, Notice, useFetch } from "../ui";

export default function Settings() {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<SettingsData>("/settings"),
  );
  const models = useFetch(() =>
    apiGet<{ models: ModelOption[] }>("/settings/engine/models"),
  );
  const [notice, setNotice] = useState<string | null>(null);

  // main session engine
  const [mainModel, setMainModel] = useState("");
  const [mainEffort, setMainEffort] = useState("low");

  // aux form state
  const [enabled, setEnabled] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [sessionHeader, setSessionHeader] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [testing, setTesting] = useState(false);

  // runtime switches
  const [rotation, setRotation] = useState(true);
  const [degradation, setDegradation] = useState(true);

  useEffect(() => {
    if (!data) return;
    setMainModel(data.engine.model);
    setMainEffort(data.engine.effort);
    setEnabled(data.aux.enabled);
    setBaseUrl(data.aux.base_url);
    setModel(data.aux.model);
    setSessionHeader(data.aux.session_header);
    setRotation(data.runtime.session_rotation_enabled);
    setDegradation(data.runtime.quota_degradation_enabled);
  }, [data]);

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const current = data!;

  function auxBody() {
    const body: Record<string, unknown> = {
      enabled,
      base_url: baseUrl,
      model,
      session_header: sessionHeader,
    };
    if (apiKey) body.api_key = apiKey;
    return body;
  }

  const modelOptions = models.data?.models ?? [];
  const modelListed = modelOptions.some((m) => m.id === mainModel);

  async function saveEngine() {
    try {
      await apiSend("PUT", "/settings/engine", { model: mainModel, effort: mainEffort });
      setNotice("已保存。会话下次启动时生效；要立刻生效，请到群管理重置对应会话。");
      await reload();
    } catch {
      setNotice("保存失败：这个模型可能不支持所选思考强度，请改选“模型默认”。");
    }
  }

  async function saveAux() {
    try {
      await apiSend("PUT", "/settings/aux", auxBody());
      setApiKey("");
      setNotice(
        enabled
          ? "网关设置已保存并生效。"
          : "已停用网关：aux 层已切到 agy 模式（独立辅助会话接管判定、打标、记忆与摘要）。",
      );
      await reload();
    } catch {
      setNotice("保存失败：请检查地址是否为 http(s) URL、模型名与会话头是否合法。");
    }
  }

  async function testAux() {
    setTesting(true);
    try {
      const result = await apiSend<{ ok: boolean; status: { error?: string | null } }>(
        "POST",
        "/settings/aux/test",
        auxBody(),
      );
      setNotice(
        result.ok
          ? "连通性测试通过：网关可达且模型可用。"
          : `连通性测试未通过：${result.status.error ?? "网关未就绪"}`,
      );
    } catch {
      setNotice("连通性测试失败：网关不可达。");
    } finally {
      setTesting(false);
    }
  }

  async function saveRuntime() {
    try {
      await apiSend("PUT", "/settings/runtime", {
        session_rotation_enabled: rotation,
        quota_degradation_enabled: degradation,
      });
      setNotice("运行开关已保存并生效。");
      await reload();
    } catch {
      setNotice("保存失败。");
    }
  }

  return (
    <div>
      <section className="card" aria-labelledby="s-engine">
        <h2 id="s-engine">主模型</h2>
        <p className="muted">
          这里决定群聊和私聊会话用的模型与思考强度。更强的模型或更高的强度通常
          消耗更多额度，也可能回复更慢。Claude 和 GPT-OSS 不能调强度，要选“模型默认”。
          辅助会话不受这里影响。
        </p>
        <label className="field">
          <span>主模型</span>
          <select value={mainModel} onChange={(e) => setMainModel(e.target.value)}>
            {!modelListed && <option value={mainModel}>{mainModel}</option>}
            {modelOptions.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}（{m.id}）
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span>思考强度</span>
          <select value={mainEffort} onChange={(e) => setMainEffort(e.target.value)}>
            <option value="">模型默认</option>
            <option value="low">low</option>
            <option value="medium">medium</option>
            <option value="high">high</option>
          </select>
        </label>
        <div className="actions">
          <button type="button" onClick={() => void saveEngine()}>
            保存主模型
          </button>
        </div>
      </section>

      <section className="card" aria-labelledby="s-aux">
        <h2 id="s-aux">辅助模型网关（aux）</h2>
        <p className="muted">
          当前模式：
          {current.aux.mode === "gateway" ? "网关模式" : "agy 模式"}。
          停用网关不会关掉辅助功能——判定、图片打标、记忆整理与泄洪摘要会
          <strong>自动落到 agy 模式</strong>，由一个独立辅助会话统一接管。
        </p>
        <label className="field inline">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          启用网关
        </label>
        <label className="field">
          <span>网关地址</span>
          <input
            type="url"
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="http://host.docker.internal:18088"
          />
        </label>
        <label className="field">
          <span>模型</span>
          <input
            type="text"
            value={model}
            onChange={(e) => setModel(e.target.value)}
          />
        </label>
        <label className="field">
          <span>粘性会话头</span>
          <input
            type="text"
            value={sessionHeader}
            onChange={(e) => setSessionHeader(e.target.value)}
          />
        </label>
        <label className="field">
          <span>
            API 密钥（{current.aux.api_key_set ? "已设置" : "未设置"}；留空表示不修改，只写不回读）
          </span>
          <input
            type="password"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            autoComplete="new-password"
          />
        </label>
        <div className="actions">
          <button type="button" onClick={() => void saveAux()}>
            保存网关设置
          </button>
          <button type="button" onClick={() => void testAux()} disabled={testing}>
            {testing ? "测试中…" : "测试连接"}
          </button>
        </div>
      </section>

      <section className="card" aria-labelledby="s-runtime">
        <h2 id="s-runtime">运行开关</h2>
        <label className="field inline">
          <input
            type="checkbox"
            checked={rotation}
            onChange={(e) => setRotation(e.target.checked)}
          />
          主会话自动轮换
        </label>
        <p className="muted">
          关掉后会话一直续用到达上限也不换新：省去轮换开销，但长会话可能逐渐漂移、
          上下文越来越旧。页面注明的代价即在此。
        </p>
        <label className="field inline">
          <input
            type="checkbox"
            checked={degradation}
            onChange={(e) => setDegradation(e.target.checked)}
          />
          按额度降级
        </label>
        <p className="muted">
          关掉后额度只显示、不干预：档位照常计算与上报（raw 档位），但不再压低
          每群的调用上限。额度真正耗尽时的行为不归这个开关管。
        </p>
        <div className="actions">
          <button type="button" onClick={() => void saveRuntime()}>
            保存运行开关
          </button>
        </div>
      </section>
      <Notice text={notice} />
    </div>
  );
}
