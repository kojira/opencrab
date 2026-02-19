import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { AppLayout } from './components/layout/AppLayout';
import Home from './pages/Home';
import Agents from './pages/Agents';
import AgentCreate from './pages/AgentCreate';
import AgentLayout from './components/layout/AgentLayout';
import AgentOverview from './pages/AgentOverview';
import AgentIdentityEdit from './pages/AgentIdentityEdit';
import PersonaEdit from './pages/PersonaEdit';
import AgentSkills from './pages/AgentSkills';
import AgentMemory from './pages/AgentMemory';
import AgentSessions from './pages/AgentSessions';
import AgentAnalytics from './pages/AgentAnalytics';
import Sessions from './pages/Sessions';
import SessionDetail from './pages/SessionDetail';
import Workspace from './pages/Workspace';

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route element={<AppLayout />}>
          <Route path="/" element={<Home />} />
          <Route path="/agents" element={<Agents />} />
          <Route path="/agents/new" element={<AgentCreate />} />
          <Route path="/agents/:id" element={<AgentLayout />}>
            <Route index element={<AgentOverview />} />
            <Route path="edit" element={<AgentIdentityEdit />} />
            <Route path="persona" element={<PersonaEdit />} />
            <Route path="skills" element={<AgentSkills />} />
            <Route path="memory" element={<AgentMemory />} />
            <Route path="sessions" element={<AgentSessions />} />
            <Route path="analytics" element={<AgentAnalytics />} />
          </Route>
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/workspace/:agentId" element={<Workspace />} />
        </Route>
      </Routes>
    </BrowserRouter>
  );
}
