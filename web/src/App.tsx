import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { AppLayout } from './components/layout/AppLayout';
import Home from './pages/Home';
import Agents from './pages/Agents';
import AgentCreate from './pages/AgentCreate';
import AgentDetail from './pages/AgentDetail';
import AgentIdentityEdit from './pages/AgentIdentityEdit';
import PersonaEdit from './pages/PersonaEdit';
import Skills from './pages/Skills';
import Memory from './pages/Memory';
import Sessions from './pages/Sessions';
import SessionDetail from './pages/SessionDetail';
import Workspace from './pages/Workspace';
import Analytics from './pages/Analytics';

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route element={<AppLayout />}>
          <Route path="/" element={<Home />} />
          <Route path="/agents" element={<Agents />} />
          <Route path="/agents/new" element={<AgentCreate />} />
          <Route path="/agents/:id" element={<AgentDetail />} />
          <Route path="/agents/:id/edit" element={<AgentIdentityEdit />} />
          <Route path="/agents/:id/persona" element={<PersonaEdit />} />
          <Route path="/skills" element={<Skills />} />
          <Route path="/memory" element={<Memory />} />
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/workspace/:agentId" element={<Workspace />} />
          <Route path="/analytics" element={<Analytics />} />
        </Route>
      </Routes>
    </BrowserRouter>
  );
}
