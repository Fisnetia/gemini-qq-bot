import { useEffect, useState } from "react";
import { apiGet, apiSend, type Persona as PersonaData } from "../api";
import { ErrorBox, Loading, Notice, useFetch } from "../ui";

interface EditorProps {
  title: string;
  path: string;
  defaultTag: string;
  resetLabel: string;
  resetConfirm: string;
  resetNotice: string;
  savedNotice: string;
}

function PersonaEditor({
  title,
  path,
  defaultTag,
  resetLabel,
  resetConfirm,
  resetNotice,
  savedNotice,
}: EditorProps) {
  const { data, error, loading, reload } = useFetch(() => apiGet<PersonaData>(path));
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
  const countId = `${path}-count`;

  async function save() {
    setSaving(true);
    try {
      await apiSend("PUT", path, { persona: draft });
      setNotice(savedNotice);
      await reload();
    } catch {
      setNotice("保存失败：内容可能为空、过长或包含不允许的标记。");
    } finally {
      setSaving(false);
    }
  }

  async function reset() {
    if (!window.confirm(resetConfirm)) return;
    setSaving(true);
    try {
      await apiSend("DELETE", path);
      setNotice(resetNotice);
      await reload();
    } catch {
      setNotice("操作失败。");
    } finally {
      setSaving(false);
    }
  }

  return (
    <>
      <h3>{title}</h3>
      <p>
        当前状态：
        {persona.is_default ? (
          <span className="tag">{defaultTag}</span>
        ) : (
          <span className="tag tag-custom">已自定义</span>
        )}
      </p>
      <label className="field">
        <span>{title}内容</span>
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          rows={14}
          aria-describedby={countId}
        />
      </label>
      <p id={countId} className={overLimit ? "bad" : "muted"}>
        {chars} / {persona.max_chars} 字{overLimit && "（超出上限，无法保存）"}
      </p>
      <div className="actions">
        <button
          type="button"
          onClick={() => void save()}
          disabled={saving || overLimit || draft.trim() === ""}
        >
          保存{title}
        </button>
        {!persona.is_default && (
          <button type="button" onClick={() => void reset()} disabled={saving}>
            {resetLabel}
          </button>
        )}
      </div>
      <Notice text={notice} />
    </>
  );
}

export default function Persona() {
  return (
    <section className="card" aria-labelledby="p-title">
      <h2 id="p-title">人设卡</h2>
      <p className="muted">
        群聊和主人私聊可以用不同的人设卡。保存后<strong>下一轮对话即生效</strong>，
        轮换后的新会话同样适用；输出契约部分是内置的，不在这里编辑。
      </p>
      <PersonaEditor
        title="群聊人设"
        path="/persona"
        defaultTag="内置默认"
        resetLabel="恢复默认"
        resetConfirm="恢复内置默认人设卡？自定义内容将被删除。"
        resetNotice="已恢复默认人设卡，下一轮生效。"
        savedNotice="已保存。各群在下一轮对话时换上新人设卡（包括轮换后的新会话）。"
      />
      <PersonaEditor
        title="私聊人设"
        path="/persona/private"
        defaultTag="未填写，沿用群聊人设"
        resetLabel="清空私聊人设"
        resetConfirm="清空私聊人设？之后私聊将沿用群聊人设。"
        resetNotice="已清空，私聊下一轮起沿用群聊人设。"
        savedNotice="已保存。私聊在下一轮对话时换上新人设卡。"
      />
    </section>
  );
}
