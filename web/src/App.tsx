import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
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
import AgentCoAgents from './pages/AgentCoAgents';
import AgentTrustedUsers from './pages/AgentTrustedUsers';
import Sessions from './pages/Sessions';
import SessionDetail from './pages/SessionDetail';
import Workspace from './pages/Workspace';
import AgentChannels from './pages/AgentChannels';
import AgentAllowedCommands from './pages/AgentAllowedCommands';
import AgentLlmLogs from './pages/AgentLlmLogs';
import SystemSettings from './pages/SystemSettings';
import Setup from './pages/Setup';

const queryClient = new QueryClient();

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
    <BrowserRouter>
      <Routes>
        <Route element={<AppLayout />}>
          <Route path="/" element={<Home />} />
          <Route path="/setup" element={<Setup />} />
          <Route path="/agents" element={<Agents />} />
          <Route path="/agents/new" element={<AgentCreate />} />
          <Route path="/agents/:id" element={<AgentLayout />}>
            <Route index element={<AgentOverview />} />
            <Route path="edit" element={<AgentIdentityEdit />} />
            <Route path="persona" element={<PersonaEdit />} />
            <Route path="skills" element={<AgentSkills />} />
            <Route path="memory" element={<AgentMemory />} />
            <Route path="sessions" element={<AgentSessions />} />
            <Route path="co-agents" element={<AgentCoAgents />} />
            <Route path="trusted-users" element={<AgentTrustedUsers />} />
            <Route path="analytics" element={<AgentAnalytics />} />
            <Route path="channels" element={<AgentChannels />} />
            <Route path="allowed-commands" element={<AgentAllowedCommands />} />
            <Route path="llm-logs" element={<AgentLlmLogs />} />
          </Route>
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/workspace/:agentId" element={<Workspace />} />
          <Route path="/settings" element={<SystemSettings />} />
        </Route>
      </Routes>
    </BrowserRouter>
    </QueryClientProvider>
  );
}
