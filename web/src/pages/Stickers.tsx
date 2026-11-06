import { useEffect, useState } from "react";
import { apiBlobUrl, apiGet, apiSend, type Sticker } from "../api";
import { ErrorBox, Loading, Notice, unixTime, useFetch } from "../ui";

interface StickersResponse {
  stickers: Sticker[];
  total: number;
  library_size: number;
}

const PAGE = 24;

export default function Stickers() {
  const [kept, setKept] = useState<string>("");
  const [offset, setOffset] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const { data, error, loading, reload } = useFetch(
    () =>
      apiGet<StickersResponse>(
        `/stickers?limit=${PAGE}&offset=${offset}${kept ? `&kept=${kept}` : ""}`,
      ),
    [kept, offset],
  );

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const { stickers, total, library_size } = data!;

  async function patch(sticker: Sticker, body: Record<string, unknown>, label: string) {
    try {
      await apiSend("PATCH", `/stickers/${sticker.hash}`, body);
      setNotice(`${label}（${sticker.short}）`);
      await reload();
    } catch {
      setNotice("操作失败");
    }
  }

  return (
    <section className="card" aria-labelledby="st-title">
      <h2 id="st-title">表情库</h2>
      <p className="muted">
        共 {total} 张，收编入库 {library_size} 张。收编的表情会被机器人当作素材使用；
        忽略的不再出现。
      </p>
      <label className="field inline">
        筛选
        <select
          value={kept}
          onChange={(e) => {
            setKept(e.target.value);
            setOffset(0);
          }}
          aria-label="按收编状态筛选"
        >
          <option value="">全部</option>
          <option value="1">已收编</option>
          <option value="0">未定</option>
          <option value="-1">已忽略</option>
        </select>
      </label>
      <Notice text={notice} />
      <div className="sticker-grid">
        {stickers.map((s) => (
          <StickerCard key={s.hash} sticker={s} onPatch={patch} onReload={reload} />
        ))}
      </div>
      {stickers.length === 0 && <p className="muted">没有符合条件的表情。</p>}
      <div className="actions">
        <button
          type="button"
          disabled={offset === 0}
          onClick={() => setOffset(Math.max(0, offset - PAGE))}
        >
          上一页
        </button>
        <button
          type="button"
          disabled={offset + PAGE >= total}
          onClick={() => setOffset(offset + PAGE)}
        >
          下一页
        </button>
      </div>
    </section>
  );
}

function StickerCard({
  sticker,
  onPatch,
  onReload,
}: {
  sticker: Sticker;
  onPatch: (s: Sticker, body: Record<string, unknown>, label: string) => Promise<void>;
  onReload: () => Promise<void>;
}) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    let url: string | null = null;
    apiBlobUrl(`/stickers/${sticker.hash}/image`)
      .then((u) => {
        url = u;
        if (live) setSrc(u);
      })
      .catch(() => undefined);
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [sticker.hash]);

  return (
    <article className="sticker" aria-label={`表情 ${sticker.short}`}>
      {src ? (
        <img src={src} alt={sticker.description ?? "表情图片"} width={96} height={96} />
      ) : (
        <div className="sticker-placeholder" aria-hidden="true">
          …
        </div>
      )}
      <p className="sticker-desc">
        {sticker.description ?? "（无描述）"}
        {sticker.emotion && <span className="tag">{sticker.emotion}</span>}
      </p>
      <p className="muted">
        见过 {sticker.seen_count} 次 · 最近 {unixTime(sticker.last_seen_at)} ·{" "}
        {sticker.kept === 1 ? "已收编" : sticker.kept === -1 ? "已忽略" : "未定"}
        {sticker.enabled ? "" : " · 已停用"}
      </p>
      <div className="actions">
        <button type="button" onClick={() => void onPatch(sticker, { kept: 1 }, "已收编")}>
          收编
        </button>
        <button type="button" onClick={() => void onPatch(sticker, { kept: 0 }, "已设为未定")}>
          未定
        </button>
        <button type="button" onClick={() => void onPatch(sticker, { kept: -1 }, "已忽略")}>
          忽略
        </button>
        <button
          type="button"
          onClick={() =>
            void onPatch(sticker, { enabled: !sticker.enabled }, sticker.enabled ? "已停用" : "已启用")
          }
        >
          {sticker.enabled ? "停用" : "启用"}
        </button>
        <button
          type="button"
          onClick={() => {
            if (window.confirm(`删除表情 ${sticker.short}？`)) {
              void apiSend("DELETE", `/stickers/${sticker.hash}`).then(onReload);
            }
          }}
        >
          删除
        </button>
      </div>
    </article>
  );
}
