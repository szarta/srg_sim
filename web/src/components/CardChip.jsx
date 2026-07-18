// A single card, rendered from the observable-state card shape
// ({ db_uuid, name, number, atk_type, play_order, ... }). Presentational only.

const ATK_TONE = {
  Strike: 'border-rose-500/60',
  Grapple: 'border-emerald-500/60',
  Submission: 'border-sky-500/60',
  None: 'border-neutral-600/60',
}

export default function CardChip({ card, selectable = false, onClick }) {
  const tone = ATK_TONE[card.atk_type] ?? ATK_TONE.None
  return (
    <div
      onClick={onClick}
      className={[
        'w-28 shrink-0 rounded-md border bg-neutral-900/80 px-2 py-1.5 text-left',
        tone,
        selectable ? 'cursor-pointer hover:bg-neutral-800' : '',
      ].join(' ')}
      title={card.raw_text || card.name}
    >
      <div className="flex items-baseline justify-between text-[10px] text-neutral-400">
        <span>#{card.number}</span>
        <span>{card.play_order}</span>
      </div>
      <div className="truncate text-sm font-medium leading-tight">{card.name}</div>
      <div className="text-[10px] text-neutral-400">{card.atk_type}</div>
    </div>
  )
}
