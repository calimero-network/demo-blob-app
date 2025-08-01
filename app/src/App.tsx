import React from 'react';
import { ProtectedRoutesWrapper } from '@calimero-network/calimero-client';
import ChatPage from './pages/ChatPage';

export default function App() {
  return (
    <ProtectedRoutesWrapper 
      permissions={["context:execute", "application"]} 
      applicationId="J3L5ws3dsMA7ru64dw2ciJ8o6mPeiTEVpgzgtKBAN4tn"
      clientApplicationPath="https://calimero-only-peers-dev.s3.amazonaws.com/uploads/364a055ddeebaa64c00c6ce013c9c8b9.wasm">
      <ChatPage />
    </ProtectedRoutesWrapper>
  );
}
