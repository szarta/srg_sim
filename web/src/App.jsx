import { useEffect, useRef, useState } from 'react'
import init, { WasmSession } from './pkg/srg_core.js'
import deckA from './sample/deckA.json'
import deckB from './sample/deckB.json'
import Board from './components/Board.jsx'
import DecisionPanel from './components/DecisionPanel.jsx'

// You (seat A) decide; seat B is a local heuristic AI that never suspends, so every
// surfaced step is your decision (or the final result). Purely presentational over
// the WASM Session + observable_state — no game logic lives here.
const SEATS = JSON.stringify({ A: 'remote', B: 'heuristic' })

export default function App() {
  const session = useRef(null)
  const [ready, setReady] = useState(false)
  const [step, setStep] = useState(null)
  const [seed, setSeed] = useState(7)
  const [error, setError] = useState(null)

  useEffect(() => {
    init()
      .then(() => setReady(true))
      .catch((e) => setError(`WASM load failed: ${e}`))
  }, [])

  const start = (s) => {
    try {
      session.current = WasmSession.open(
        JSON.stringify(deckA),
        JSON.stringify(deckB),
        SEATS,
        BigInt(s),
      )
      setStep(JSON.parse(session.current.step()))
      setError(null)
    } catch (e) {
      setError(String(e))
    }
  }

  useEffect(() => {
    if (ready) start(seed)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready])

  const submit = (i) => {
    try {
      setStep(JSON.parse(session.current.submit(i)))
    } catch (e) {
      setError(String(e))
    }
  }

  return (
    <div className="mx-auto max-w-4xl p-4">
      <Header seed={seed} setSeed={setSeed} onNewMatch={() => start(seed)} disabled={!ready} />
      {error && (
        <div className="mb-3 rounded border border-rose-600 bg-rose-950/60 p-2 text-sm text-rose-200">
          {error}
        </div>
      )}
      {!ready || !step ? <div className="text-neutral-400">Loading engine…</div> : <Match step={step} onSubmit={submit} />}
    </div>
  )
}

function Header({ seed, setSeed, onNewMatch, disabled }) {
  return (
    <header className="mb-4 flex items-center justify-between">
      <h1 className="text-lg font-semibold">
        SRG Supershow <span className="text-neutral-500">— {deckA.competitor.name} vs {deckB.competitor.name}</span>
      </h1>
      <div className="flex items-center gap-2 text-sm">
        <label className="text-neutral-400">seed</label>
        <input
          type="number"
          value={seed}
          onChange={(e) => setSeed(Number(e.target.value))}
          className="w-20 rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
        />
        <button
          onClick={onNewMatch}
          disabled={disabled}
          className="rounded border border-neutral-600 bg-neutral-800 px-3 py-1 hover:bg-neutral-700 disabled:opacity-50"
        >
          New match
        </button>
      </div>
    </header>
  )
}

function Match({ step, onSubmit }) {
  if (step.kind === 'done') return <Result result={step.result} />

  const req = step.request
  const obs = req.observable_state
  const active = obs.active
  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between text-sm text-neutral-400">
        <span>turn {obs.turn_no}</span>
        <span>
          crowd meter <span className="font-mono text-neutral-200">{fmt(obs.crowd_meter)}</span>
        </span>
        <span>active {active}</span>
      </div>
      <Board label="Opponent" view={obs.players.B} isSelf={false} isActive={active === 'B'} />
      <Board label="You" view={obs.players.A} isSelf isActive={active === 'A'} />
      <DecisionPanel request={req} onSubmit={onSubmit} />
    </div>
  )
}

function Result({ result }) {
  const you = result.winner === 'A'
  return (
    <div className="rounded-lg border border-neutral-700 bg-neutral-900 p-6 text-center">
      <div className={`text-2xl font-bold ${you ? 'text-emerald-400' : 'text-rose-400'}`}>
        {result.winner === 'draw' ? 'Draw' : you ? 'You win' : 'You lose'}
      </div>
      <div className="mt-1 text-neutral-400">
        by {result.reason} in {result.turns} turns
      </div>
    </div>
  )
}

const fmt = (n) => (n > 0 ? `+${n}` : `${n}`)
