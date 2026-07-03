import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import './styles/global.css'
import './components/root.css'

const root = document.getElementById('root')
if (!root) throw new Error('missing #root mount')
createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
