import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// Same stack as the card-search frontend (Vite + React + Tailwind v4).
export default defineConfig({
  plugins: [react(), tailwindcss()],
})
