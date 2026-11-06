import { useEffect, useState } from "react";
import { apiGet, apiSend, type Persona as PersonaData } from "../api";
import { ErrorBox, Loading, Notice, useFetch } from "../ui";

export default function Persona() {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<PersonaData>("/persona"),
  );
  const [draft, setDraft] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (data) setDraft(data.persona);
  }, [data]);

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const persona = data!;
  const chars = [...draft].length;
  const overLimit = chars > persona.max_chars;

  async function save() {
    setSaving(true);
    try {
      await apiSend("PUT", "/persona", { persona: draft });
      setNotice("已保存。各群在下一轮对话时换上新人设卡（包括轮换后的新会话）。");
      await reload();
    } catch {
      setNotice("保存失败：内容可能为空、过长或包含不允许的标记。");
    } finally {
      setSaving(false);
    }
  }

  async function reset() {
    if (!window.confirm("恢复内置默认人设卡？自定义内容将被删除。")) return;
    setSaving(true);
    try {
      await apiSend("DELETE", "/persona");
      setNotice("已恢复默认人设卡，下一轮生效。");
      await reload();
    } catch {
      setNotice("恢复失败。");
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="card" aria-labelledby="p-title">
      <h2 id="p-title">人设卡</h2>
      <p className="muted">
        这张卡是机器人的人格底稿，随每群的 AGENTS.md 一起被读取。保存后
        <strong>下一轮对话即生效</strong>（各群在自己的下一轮换卡，轮换新会话同样适用）；
        输出契约部分是内置的，不在这里编辑。
      </p>
      <p>
        当前状态：
        {persona.is_default ? (
          <span className="tag">内置默认</span>
        ) : (
          <span className="tag tag-custom">已自定义</span>
        )}
      </p>
      <label className="field">
        <span>人设卡内容</span>
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          rows={18}
          aria-describedby="p-count"
        />
      </label>
      <p id="p-count" className={overLimit ? "bad" : "muted"}>
        {chars} / {persona.max_chars} 字{overLimit && "（超出上限，无法保存）"}
      </p>
      <div className="actions">
        <button type="button" onClick={() => void save()} disabled={saving || overLimit}>
          保存人设卡
        </button>
        {!persona.is_default && (
          <button type="button" onClick={() => void reset()} disabled={saving}>
            恢复默认
          </button>
        )}
      </div>
      <Notice text={notice} />
    </section>
  );
}
